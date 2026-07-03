//! User-facing plugin traits + internal bridge.
//!
//! This crate is the plugin author's entry point. The single
//! `impl PluginLogic for MyPlugin { ... }` block covers both
//! audio-thread DSP and main-thread GUI, with sample precision
//! routed through the prelude (see `truce::prelude` /
//! `truce::prelude64`).
//!
//! `truce-plugin` depends on `truce-gui-types` (light: layout,
//! render trait, widget regions) - not the full `truce-gui`.
//! Plugin authors who supply a custom editor (egui, iced, slint,
//! raw window handle) end up with `truce-plugin` in their dep
//! tree but not the built-in editor's tiny-skia + baseview +
//! truce-font stack.
//!
//! ## Three traits, one source of truth
//!
//! - [`PluginLogic`]   - what plugin authors implement for `f32`-buffer plugins.
//! - [`PluginLogic64`] - what plugin authors implement for `f64`-buffer plugins.
//! - [`PluginLogicCore`] - generic-over-`S` trait the format wrappers consume.
//!
//! The two leaf traits are stamped from one
//! `plugin_logic_leaf_trait!` `macro_rules!` definition (further
//! down this file) so their method surfaces stay in lock-step. Each leaf
//! gets a blanket impl that forwards every method to
//! `PluginLogicCore<S>` with the matching `S`. Wrappers
//! (`StaticShell`, `HotShell`, the format crates) bind on
//! `PluginLogicCore<S>` and don't care which leaf the user impl'd.
//!
//! ## What this buys
//!
//! Plugin authors writing `impl PluginLogic for Synth { ... }`
//! never name a precision. The `truce::prelude64` re-export aliases
//! `PluginLogic64` as `PluginLogic` in the user's scope, so the
//! same impl header reads the same regardless of which prelude is
//! in use. The `<S>` token that used to live on the impl header is
//! gone - the prelude carries the precision choice.

use truce_core::buffer::AudioBuffer;
use truce_core::bus::BusLayout;
use truce_core::denormal::DenormalGuard;
use truce_core::editor::Editor;
use truce_core::events::EventList;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_core::state::{ForeignState, MigratedState, StateLoadError};
use truce_gui_types::interaction::WidgetRegion;
use truce_gui_types::widgets::WidgetType;
use truce_params::sample::Sample;

// ---------------------------------------------------------------------------
// PluginLogicCore - generic trait, what format wrappers consume
// ---------------------------------------------------------------------------

/// Wrapper-facing plugin trait, generic over the audio sample type.
///
/// Format wrappers (`StaticShell`, `HotShell`, CLAP / VST3 / etc.)
/// bind on `PluginLogicCore<S>`. Plugin authors don't implement this
/// directly - they implement [`PluginLogic`] (`f32`) or
/// [`PluginLogic64`] (`f64`), and the blanket impls below route them
/// into `PluginLogicCore`.
///
/// Method docs live on the leaf traits ([`PluginLogic`] /
/// [`PluginLogic64`]); the shape mirrors them exactly.
pub trait PluginLogicCore<S: Sample = f32>: Send + 'static {
    #[must_use]
    fn supports_in_place() -> bool
    where
        Self: Sized;

    #[must_use]
    fn bus_layouts() -> Vec<BusLayout>
    where
        Self: Sized;

    fn reset(&mut self, sample_rate: f64, max_block_size: usize);

    fn process(
        &mut self,
        buffer: &mut AudioBuffer<S>,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus;

    fn save_state(&self) -> Vec<u8>;
    /// Restore plugin-specific state. See [`PluginLogic::load_state`].
    ///
    /// # Errors
    ///
    /// Forwards whatever the user impl returns - typically a malformed
    /// blob error decoded by `bincode` / `serde` / similar.
    fn load_state(&mut self, data: &[u8]) -> Result<(), StateLoadError>;
    /// Translate foreign state into truce shape. See
    /// [`PluginLogic::migrate_state`].
    #[must_use]
    fn migrate_state(foreign: &ForeignState) -> Option<MigratedState>
    where
        Self: Sized;
    fn state_changed(&mut self);
    fn latency(&self) -> u32;
    fn tail(&self) -> u32;
    /// Construct the editor for this plugin. Required - there is no
    /// auto-fallback. Layout-only plugins call
    /// `truce_gui::default_editor(params, layout)` from here; custom-
    /// renderer plugins construct their `EguiEditor` / `IcedEditor` /
    /// `SlintEditor` / hand-rolled `Editor` directly.
    fn editor(&self) -> Box<dyn Editor>;
}

// ---------------------------------------------------------------------------
// Leaf traits - what plugin authors implement
// ---------------------------------------------------------------------------

/// Define a sample-pinned leaf trait. Two invocations:
/// `PluginLogic` (f32) and [`PluginLogic64`] (f64). The trait
/// definition has to be a macro because we want the two trait
/// surfaces to stay in exact lock-step - adding a new method means
/// updating one place, not three (the macro, plus two trait
/// declarations).
///
/// Doc-hidden because it's a single-purpose internal macro, not an
/// API users should reach for.
#[doc(hidden)]
#[macro_export]
macro_rules! plugin_logic_leaf_trait {
    ($(#[$attr:meta])* $vis:vis trait $name:ident<sample = $sample:ty>) => {
        $(#[$attr])*
        $vis trait $name: Send + 'static {
            /// Opt into zero-copy in-place I/O. When this returns `true`,
            /// the format wrapper skips its safety memcpy on host-aliased
            /// buffers and hands the plugin the raw shared memory through
            /// `AudioBuffer::in_out_mut(ch)`. The plugin must check
            /// `AudioBuffer::is_in_place(ch)` per channel before reading
            /// `input(ch)`.
            ///
            /// Default `false`: the wrapper copies aliased inputs into
            /// scratch so `input(ch)` and `output(ch)` are always
            /// disjoint. Costs one memcpy per aliased channel per block.
            #[must_use]
            fn supports_in_place() -> bool
            where
                Self: Sized,
            {
                false
            }

            /// Supported audio bus configurations. The host picks one;
            /// the others are rejected at bus-config time before
            /// `process` is ever called. Default: stereo in, stereo out.
            #[must_use]
            fn bus_layouts() -> Vec<$crate::__plugin_logic_deps::BusLayout>
            where
                Self: Sized,
            {
                vec![$crate::__plugin_logic_deps::BusLayout::stereo()]
            }

            /// Reset for a new sample rate / block size. Called before
            /// the first `process` and any time the host reconfigures.
            fn reset(&mut self, sample_rate: f64, max_block_size: usize);

            /// Process one block of audio. Real-time - no allocations,
            /// locks, or I/O.
            fn process(
                &mut self,
                buffer: &mut $crate::__plugin_logic_deps::AudioBuffer<$sample>,
                events: &$crate::__plugin_logic_deps::EventList,
                context: &mut $crate::__plugin_logic_deps::ProcessContext,
            ) -> $crate::__plugin_logic_deps::ProcessStatus;

            /// Serialize plugin-specific state (DSP state, not params -
            /// those are saved automatically). Default: no extra state.
            ///
            /// Runs on a host or GUI thread while the audio thread is
            /// paused at a block boundary (the wrapper's plugin lock),
            /// so reading any field is safe - but an audio block that
            /// arrives mid-save waits for this to return. Keep it
            /// cheap: copy bytes out, don't compute or compress here.
            fn save_state(&self) -> Vec<u8> {
                Vec::new()
            }

            /// Restore plugin-specific state.
            ///
            /// Runs on the audio thread between blocks, with the same
            /// exclusive access `process()` has - writing any field
            /// is safe.
            ///
            /// # Errors
            ///
            /// Return `Err(StateLoadError)` when the blob is malformed
            /// or otherwise can't be interpreted - the format wrapper
            /// logs the failure (and on hosts that support it, surfaces
            /// it to the DAW).
            fn load_state(
                &mut self,
                _data: &[u8],
            ) -> Result<(), $crate::__plugin_logic_deps::StateLoadError> {
                Ok(())
            }

            /// Called on the audio thread immediately after
            /// [`Self::load_state`] returns. Invalidate or recompute any
            /// caches the next `process()` reads. Default: no-op.
            fn state_changed(&mut self) {}

            /// Translate foreign state - a previous framework's blob,
            /// or a truce envelope saved under a different plugin id -
            /// into truce params + extra, so a plugin ported to truce
            /// keeps its users' old sessions and presets. Runs on the
            /// host thread; receiverless so it can't touch (or alias)
            /// the live instance. Return `None` for bytes you don't
            /// recognize - the wrapper then reports load failure to
            /// the host, exactly as if this hook didn't exist.
            ///
            /// One-shot by construction: the next save writes a normal
            /// truce envelope, so this never becomes a permanent
            /// dual-format reader. Keyed formats (AU / LV2 / AAX) only
            /// see foreign bytes when `truce.toml` declares the legacy
            /// keys to probe (`[plugin.legacy_state]`).
            #[must_use]
            fn migrate_state(
                _foreign: &$crate::__plugin_logic_deps::ForeignState,
            ) -> Option<$crate::__plugin_logic_deps::MigratedState>
            where
                Self: Sized,
            {
                None
            }

            /// Report latency in samples for plugin delay compensation.
            fn latency(&self) -> u32 {
                0
            }

            /// Report tail time in samples (audio produced after input
            /// stops - reverbs, delays). `u32::MAX` for infinite tail.
            fn tail(&self) -> u32 {
                0
            }

            // ---- GUI ----

            /// Construct the editor for this plugin. Required.
            ///
            /// There is no auto-fallback - every plugin explicitly
            /// names which renderer it wants. For the built-in
            /// widget layout, call
            /// `truce_gui::default_editor(params, layout)`; for
            /// custom renderers, construct an `EguiEditor` /
            /// `IcedEditor` / `SlintEditor` / hand-rolled `Editor`
            /// here. The choice of renderer crate the plugin's
            /// `Cargo.toml` pulls IS the choice of editor.
            fn editor(&self) -> Box<dyn $crate::__plugin_logic_deps::Editor>;
        }
    };
}

// Re-export the dependencies the leaf-trait macro substitutes by path,
// under one `pub` doc-hidden module so user crates that invoke the
// macro don't need to import each truce-core type by hand.
#[doc(hidden)]
pub mod __plugin_logic_deps {
    pub use truce_core::buffer::AudioBuffer;
    pub use truce_core::bus::BusLayout;
    pub use truce_core::editor::Editor;
    pub use truce_core::events::EventList;
    pub use truce_core::process::{ProcessContext, ProcessStatus};
    pub use truce_core::state::{ForeignState, MigratedState, StateLoadError};
}

plugin_logic_leaf_trait! {
    /// The `f32`-buffer user-facing plugin trait.
    ///
    /// Plugin authors implement this in a single `impl` block when
    /// their audio path is `f32` end-to-end (the default - matches
    /// the host wire format for nearly all DAWs and formats).
    /// `truce::prelude` and `truce::prelude32` re-export this name
    /// directly; `truce::prelude64m` does too (the `m` mixed-precision
    /// prelude keeps the audio buffer at `f32` and only switches the
    /// `param.read()` precision).
    ///
    /// Required: [`Self::reset`], [`Self::process`], [`Self::editor`].
    /// Everything else has a default. The editor is constructed
    /// explicitly - layout-only plugins typically call
    /// `truce_gui::default_editor(self.params.clone(), self.layout())`
    /// (where `layout()` is a plain inherent method on the plugin
    /// struct, not part of the trait).
    pub trait PluginLogic<sample = f32>
}

plugin_logic_leaf_trait! {
    /// The `f64`-buffer user-facing plugin trait. Same surface as
    /// [`PluginLogic`] but with the audio buffer pinned to `f64`.
    ///
    /// Plugin authors don't usually name this directly - `truce::prelude64`
    /// re-exports it as `PluginLogic`, so the impl header reads the
    /// same regardless of which precision the prelude chose. Pick
    /// `truce::prelude64` (and thus this leaf) when the DSP path runs
    /// in `f64` end-to-end and the wrapper-boundary widen/narrow
    /// memcpy is worth the cleaner DSP code.
    pub trait PluginLogic64<sample = f64>
}

// ---------------------------------------------------------------------------
// Bridges - each leaf forwards every method to PluginLogicCore<S>
// ---------------------------------------------------------------------------

/// Define a blanket `impl<T: $leaf> PluginLogicCore<$sample> for T`
/// that forwards every trait method to `<T as $leaf>::method(...)`.
/// One source-of-truth for both `(PluginLogic, f32)` and
/// `(PluginLogic64, f64)` bridges.
macro_rules! plugin_logic_bridge {
    ($leaf:ident, $sample:ty) => {
        impl<T: $leaf> PluginLogicCore<$sample> for T {
            fn supports_in_place() -> bool
            where
                Self: Sized,
            {
                <Self as $leaf>::supports_in_place()
            }

            fn bus_layouts() -> Vec<BusLayout>
            where
                Self: Sized,
            {
                <Self as $leaf>::bus_layouts()
            }

            fn reset(&mut self, sample_rate: f64, max_block_size: usize) {
                <Self as $leaf>::reset(self, sample_rate, max_block_size);
            }

            fn process(
                &mut self,
                buffer: &mut AudioBuffer<$sample>,
                events: &EventList,
                context: &mut ProcessContext,
            ) -> ProcessStatus {
                // FTZ/DAZ (or FZ on AArch64) for the duration of
                // the user's process body. Denormals on filter
                // feedback paths stall the core; the guard pays
                // ~two MXCSR writes per block to avoid that.
                let _denormal_guard = DenormalGuard::new();
                <Self as $leaf>::process(self, buffer, events, context)
            }

            fn save_state(&self) -> Vec<u8> {
                <Self as $leaf>::save_state(self)
            }

            fn load_state(&mut self, data: &[u8]) -> Result<(), StateLoadError> {
                <Self as $leaf>::load_state(self, data)
            }

            fn state_changed(&mut self) {
                <Self as $leaf>::state_changed(self);
            }

            fn migrate_state(foreign: &ForeignState) -> Option<MigratedState>
            where
                Self: Sized,
            {
                <Self as $leaf>::migrate_state(foreign)
            }

            fn latency(&self) -> u32 {
                <Self as $leaf>::latency(self)
            }

            fn tail(&self) -> u32 {
                <Self as $leaf>::tail(self)
            }

            fn editor(&self) -> Box<dyn Editor> {
                <Self as $leaf>::editor(self)
            }
        }
    };
}

plugin_logic_bridge!(PluginLogic, f32);
plugin_logic_bridge!(PluginLogic64, f64);

// ---------------------------------------------------------------------------
// Default hit test - referenced by leaf macro expansions
// ---------------------------------------------------------------------------

/// Default hit test: circular for knobs, rectangular for everything
/// else, skip meters. Used by the leaf traits' `hit_test` defaults.
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
