//! The DSP-only user-facing trait. Plugin authors implement this
//! plus the GUI-side counterpart `truce_gui::PluginEditor`. The
//! `truce::plugin!` macro bridges the pair into [`crate::Plugin`]
//! for format wrappers.
//!
//! Lives here, not in `truce-loader`, because the trait is
//! everyone-implements regardless of hot-reload, and the loader
//! crate's role is hot-reload mechanics — `dlopen`, ABI canary,
//! vtable probe — not user-facing API. The split (DSP in
//! `truce-core`, GUI in `truce-gui`) keeps headless plugins from
//! pulling GUI types into compile errors or rustdoc.

use crate::buffer::AudioBuffer;
use crate::bus::BusLayout;
use crate::events::EventList;
use crate::process::{ProcessContext, ProcessStatus};

/// The DSP surface every plugin implements.
///
/// Construction (`new()`) is an inherent method on each plugin
/// struct, not part of this trait. The `truce::plugin!` macro
/// calls it with `Arc<Params>` so the plugin shares params with
/// the shell and GUI.
///
/// All methods use safe Rust types. No `unsafe`, no `#[repr(C)]`,
/// no raw pointers.
pub trait PluginLogic: Send + 'static {
    /// Opt into zero-copy in-place I/O. See
    /// [`crate::Plugin::supports_in_place`] for the full contract.
    ///
    /// `where Self: Sized` so the trait stays dyn-compatible —
    /// hot-reload wraps `Box<dyn LoaderPlugin>` and would lose
    /// dyn dispatch if any method took `Self` by value.
    #[must_use]
    fn supports_in_place() -> bool
    where
        Self: Sized,
    {
        false
    }

    /// Supported audio bus configurations. The host picks one;
    /// the others are rejected at bus-config time before
    /// `process` is ever called.
    ///
    /// Default: stereo in, stereo out. Override for instruments
    /// (no input), sidechain (extra input), multi-out, etc.
    #[must_use]
    fn bus_layouts() -> Vec<BusLayout>
    where
        Self: Sized,
    {
        vec![BusLayout::stereo()]
    }

    /// Reset for a new sample rate / block size. Called before
    /// the first `process` and any time the host reconfigures.
    fn reset(&mut self, sample_rate: f64, max_block_size: usize);

    /// Process one block of audio. Real-time — no allocations,
    /// locks, or I/O.
    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus;

    /// Serialize plugin-specific state (DSP state, not params —
    /// those are saved automatically). Default: no extra state.
    fn save_state(&self) -> Vec<u8> {
        Vec::new()
    }

    /// Restore plugin-specific state.
    fn load_state(&mut self, _data: &[u8]) {}

    /// Called on the audio thread immediately after [`Self::load_state`]
    /// returns. Use this to invalidate or recompute caches the next
    /// `process()` block reads — decoded IRs, sample thumbnails,
    /// computed pad layouts — anything derived from the extra-state
    /// blob that isn't itself part of the saved bytes.
    ///
    /// Runs under the same `&mut self` borrow that just executed
    /// `load_state`, so the next audio block sees the refreshed
    /// caches. Default: no-op.
    ///
    /// The companion [`crate::Editor::state_changed`] (on the GUI
    /// side) is fired separately by the format wrappers when a
    /// custom editor is open and the host loads state. The two
    /// hooks split plugin-thread cache invalidation from
    /// GUI-thread repaint.
    fn state_changed(&mut self) {}

    /// Report latency in samples for plugin delay compensation.
    fn latency(&self) -> u32 {
        0
    }

    /// Report tail time in samples (audio produced after input
    /// stops — reverbs, delays). `u32::MAX` for infinite tail.
    fn tail(&self) -> u32 {
        0
    }
}
