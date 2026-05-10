//! `StaticShell` — embeds the plugin directly into the binary.
//!
//! No dlopen, no file watcher, no Mutex. Same types as `HotShell`
//! but zero runtime overhead. Use via `export_static!`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use truce_core::PluginLogic;
use truce_core::buffer::AudioBuffer;
use truce_core::bus::BusLayout;
use truce_core::editor::Editor;
use truce_core::events::{EventBody, EventList};
use truce_core::info::PluginInfo;
use truce_core::plugin::Plugin;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_gui::PluginEditor;
use truce_params::Params;

// ---------------------------------------------------------------------------
// StaticShell
// ---------------------------------------------------------------------------

/// A static plugin shell that embeds `PluginLogic` directly.
///
/// Same bridging as `HotShell` but without `NativeLoader`, `Mutex`,
/// file watching, or any dynamic loading overhead. Use via `export_static!`.
pub struct StaticShell<P: Params, L: PluginLogic + PluginEditor> {
    pub params: Arc<P>,
    logic: L,
    meters: Arc<[AtomicU32; 256]>,
    sample_rate: f64,
}

unsafe impl<P: Params, L: PluginLogic + PluginEditor> Send for StaticShell<P, L> {}

impl<P: Params + Default + 'static, L: PluginLogic + PluginEditor + 'static> StaticShell<P, L> {
    /// Create from pre-constructed parts. The plugin logic should
    /// hold an `Arc::clone` of the same params.
    pub fn from_parts(params: Arc<P>, logic: L) -> Self {
        Self {
            params,
            logic,
            meters: Arc::new(std::array::from_fn(|_| AtomicU32::new(0))),
            sample_rate: 44100.0,
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

    /// Try to get a custom editor from the plugin logic.
    pub fn try_custom_editor(&self) -> Option<Box<dyn Editor>> {
        self.logic.custom_editor()
    }

    /// Try to create a `BuiltinEditor` from the plugin's layout.
    /// Returns `None` if the layout has zero size.
    pub fn try_builtin_editor(&self) -> Option<truce_gui::editor::BuiltinEditor<P>> {
        let layout = self.logic.layout();
        if layout.width == 0 || layout.height == 0 {
            return None;
        }
        Some(truce_gui::editor::BuiltinEditor::new_grid(
            Arc::clone(&self.params),
            layout,
        ))
    }
}

impl<P: Params + Default + 'static, L: PluginLogic + PluginEditor + 'static> Plugin
    for StaticShell<P, L>
{
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
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Apply parameter change events to the shell's params.
        // ParamChange values from format wrappers are PLAIN (already denormalized).
        // Using set_normalized here would double-denormalize. (Regression: see param_sync_test)
        for e in events.iter() {
            if let EventBody::ParamChange { id, value } = &e.body {
                self.params.set_plain(*id, *value);
            }
        }

        // No sync needed — plugin reads from the same Arc<Params>.

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

    fn save_state(&self) -> Option<Vec<u8>> {
        let data = self.logic.save_state();
        if data.is_empty() { None } else { Some(data) }
    }

    fn load_state(&mut self, data: &[u8]) {
        self.logic.load_state(data);
        // Plugin-side cache invalidation runs in the same `&mut`
        // borrow window so the next `process()` block sees the
        // refreshed caches. See `PluginEditor::state_changed`.
        self.logic.state_changed();
    }

    fn editor(&mut self) -> Option<Box<dyn Editor>> {
        if let Some(editor) = self.logic.custom_editor() {
            return Some(editor);
        }
        self.try_builtin_editor()
            .map(|e| Box::new(truce_gpu::GpuEditor::new(e)) as Box<dyn Editor>)
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
        // panicking — they fall through to the `get` -> None path.
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

/// Compile-time static embedding of a plugin (`PluginLogic` +
/// `PluginEditor`) into the binary.
///
/// Produces a `__HotShellWrapper` struct that implements `Plugin + PluginExport`,
/// so format export macros (`export_clap!`, `export_vst3!`, etc.) work unchanged.
/// No dlopen, no file watcher, zero runtime overhead. Bus layouts come from
/// `<$logic as PluginLogic>::bus_layouts()` — override the trait method to
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
            inner: $crate::static_shell::StaticShell<$params, $logic>,
        }

        impl $crate::__macro_deps::truce_core::plugin::Plugin for __HotShellWrapper {
            fn supports_in_place() -> bool where Self: Sized {
                <$logic as $crate::__macro_deps::truce_core::PluginLogic>::supports_in_place()
            }

            fn info() -> $crate::__macro_deps::truce_core::info::PluginInfo where Self: Sized {
                $info
            }

            fn bus_layouts() -> Vec<$crate::__macro_deps::truce_core::bus::BusLayout> where Self: Sized {
                <$logic as $crate::__macro_deps::truce_core::PluginLogic>::bus_layouts()
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
