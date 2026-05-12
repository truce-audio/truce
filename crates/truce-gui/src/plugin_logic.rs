//! The unified user-facing plugin trait.
//!
//! Plugin authors implement [`PluginLogic`] in a single `impl` block; the
//! `truce::plugin!` macro bridges into [`truce_core::Plugin`] for
//! format wrappers. Lives in `truce-gui` (not `truce-core`) because
//! the GUI methods reference `truce-gui` types — `truce-core` stays
//! GUI-free for the format-wrapper-facing surface.

use truce_core::buffer::AudioBuffer;
use truce_core::bus::BusLayout;
use truce_core::editor::Editor;
use truce_core::events::EventList;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_core::state::StateLoadError;
use truce_params::sample::Sample;

use crate::interaction::WidgetRegion;
use crate::layout::GridLayout;
use crate::render::RenderBackend;
use crate::widgets::WidgetType;

/// The user-facing plugin trait. One trait, one `impl` block —
/// covers both the audio-thread DSP surface and the main-thread GUI
/// surface.
///
/// Construction (`new()`) is an inherent method on each plugin
/// struct, not part of this trait. The `truce::plugin!` macro calls
/// it with `Arc<Params>` so the plugin shares params with the shell
/// and GUI.
///
/// Only [`Self::reset`] and [`Self::process`] are required;
/// everything else has a default. Headless (no-GUI) plugins leave
/// `layout` / `render` / `custom_editor` at their defaults — the
/// format wrappers fall back to a minimal built-in editor that
/// reads parameters every frame.
pub trait PluginLogic<S: Sample = f32>: Send + 'static {
    // ---- DSP / lifecycle ----

    /// Opt into zero-copy in-place I/O. When this returns `true`,
    /// the format wrapper skips its safety memcpy on host-aliased
    /// buffers and hands the plugin the raw shared memory through
    /// `AudioBuffer::in_out_mut(ch)`. The plugin must check
    /// `AudioBuffer::is_in_place(ch)` per channel before reading
    /// `input(ch)` — for in-place channels `input(ch)` returns an
    /// empty slice, and the data lives only in the shared buffer.
    ///
    /// Default `false`: the wrapper copies aliased inputs into scratch
    /// so `input(ch)` and `output(ch)` are always disjoint. Costs one
    /// memcpy per aliased channel per block (a few hundred KB/sec at
    /// audio rates) and lets plugin code stay format-agnostic.
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
    ///
    /// The buffer's element type `S` follows the prelude:
    /// `prelude` / `prelude32` → `f32`; `prelude64` → `f64`. Plugins
    /// that pick `f64` get the wrapper to widen/narrow at the block
    /// boundary so user code stays cast-free; `prelude64m` keeps
    /// `S = f32` (and reads params at `f64` for stable math).
    fn process(
        &mut self,
        buffer: &mut AudioBuffer<'_, S>,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus;

    /// Serialize plugin-specific state (DSP state, not params —
    /// those are saved automatically). Default: no extra state.
    fn save_state(&self) -> Vec<u8> {
        Vec::new()
    }

    /// Restore plugin-specific state.
    ///
    /// # Errors
    ///
    /// Return `Err(StateLoadError)` when the blob is malformed or
    /// otherwise can't be interpreted — the format wrapper logs the
    /// failure (and on hosts that support it, surfaces it to the DAW).
    fn load_state(&mut self, _data: &[u8]) -> Result<(), StateLoadError> {
        Ok(())
    }

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
    /// The companion [`truce_core::Editor::state_changed`] (on the
    /// boxed editor returned from `custom_editor`) is fired
    /// separately by the format wrappers when a custom editor is open
    /// and the host loads state. The two hooks split plugin-thread
    /// cache invalidation from GUI-thread repaint.
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

    // ---- GUI ----

    /// Return the widget layout for the built-in GUI. Default:
    /// empty layout (`GridLayout::build(vec![])`). Plugins that
    /// supply a custom editor via [`Self::custom_editor`] can
    /// leave this default — the format wrappers prefer the
    /// custom editor when present.
    fn layout(&self) -> GridLayout {
        GridLayout::build(vec![])
    }

    /// Render the GUI into a backend. Default: no-op. Override
    /// only for custom GPU/CPU rasterisation outside the standard
    /// widget set; flip [`Self::uses_custom_render`] to `true`
    /// when you do.
    fn render(&self, _backend: &mut dyn RenderBackend) {}

    /// Whether this plugin overrides [`Self::render`]. Default:
    /// `false`. The shell uses the standard widget drawing from
    /// [`Self::layout`] when this is `false`.
    fn uses_custom_render(&self) -> bool {
        false
    }

    /// Hit test: which widget (if any) is at `(x, y)`? Default:
    /// the standard hit-test (circular for knobs, rectangular
    /// for others, meters skipped).
    fn hit_test(&self, widgets: &[WidgetRegion], x: f32, y: f32) -> Option<usize> {
        default_hit_test(widgets, x, y)
    }

    /// Provide a custom [`Editor`] instead of the built-in widget
    /// layout (egui, iced, slint, raw window handle). Default:
    /// `None`. The shell calls this first; if it returns `None`,
    /// the shell falls back to the built-in editor from
    /// [`Self::layout`].
    fn custom_editor(&self) -> Option<Box<dyn Editor>> {
        None
    }
}

/// Default hit test: circular for knobs, rectangular for
/// everything else, skip meters. Used by [`PluginLogic::hit_test`]'s
/// default impl.
#[must_use]
pub fn default_hit_test(widgets: &[WidgetRegion], x: f32, y: f32) -> Option<usize> {
    for (i, w) in widgets.iter().enumerate() {
        if w.widget_type == WidgetType::Meter {
            continue;
        }
        if w.widget_type == WidgetType::Knob {
            let dx = x - w.cx;
            let dy = y - w.cy;
            if dx * dx + dy * dy <= w.radius * w.radius {
                return Some(i);
            }
        } else if x >= w.x && x <= w.x + w.w && y >= w.y && y <= w.y + w.h {
            return Some(i);
        }
    }
    None
}
