use crate::buffer::AudioBuffer;
use crate::bus::BusLayout;
use crate::editor::Editor;
use crate::events::EventList;
use crate::info::PluginInfo;
use crate::process::{ProcessContext, ProcessStatus};

/// The core trait that all plugins implement.
pub trait Plugin: Send + 'static {
    /// Static metadata about the plugin.
    ///
    /// Use `plugin_info!()` for zero-boilerplate (reads from truce.toml
    /// + Cargo.toml). Requires `truce-build` in `[build-dependencies]`.
    fn info() -> PluginInfo
    where
        Self: Sized;

    /// Supported bus layouts. The host picks one.
    fn bus_layouts() -> Vec<BusLayout>
    where
        Self: Sized,
    {
        vec![BusLayout::stereo()]
    }

    /// Called once after construction. Not real-time safe.
    fn init(&mut self) {}

    /// Called when sample rate or max block size changes.
    /// Reset filters, delay lines, etc. Not real-time safe.
    fn reset(&mut self, sample_rate: f64, max_block_size: usize);

    /// Real-time audio processing.
    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus;

    /// Save extra state beyond parameter values.
    fn save_state(&self) -> Option<Vec<u8>> {
        None
    }

    /// Restore extra state.
    fn load_state(&mut self, _data: &[u8]) {}

    /// GUI editor. Return None for headless plugins.
    fn editor(&mut self) -> Option<Box<dyn Editor>> {
        None
    }

    /// Processing latency in samples. Host uses this for delay compensation.
    /// Return 0 if the plugin adds no latency (default).
    fn latency(&self) -> u32 {
        0
    }

    /// Tail time in samples. Return u32::MAX for infinite tail.
    /// Return 0 for no tail (default).
    fn tail(&self) -> u32 {
        0
    }

    /// Read a meter value by ID (0.0–1.0). Called by the GUI at ~60fps.
    /// Override to expose level meters, gain reduction, etc.
    fn get_meter(&self, _meter_id: u32) -> f32 {
        0.0
    }
}
