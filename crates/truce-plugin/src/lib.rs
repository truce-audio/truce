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
use truce_core::config::AudioConfig;
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
pub trait PluginLogicCore<S: Sample = f32>: 'static {
    /// The plugin's parameter struct; mirrors the leaf's `Params`.
    type Params: truce_params::Params;
    /// The mutable per-block audio state. Owned by the shell, not by
    /// `Self` (the descriptor). `Send` because the shell moves it across
    /// threads; `'static` because the shell may outlive any borrow. No
    /// layout trait is required: the hot-reload shell fingerprints the
    /// type at load time from its `type_name` / `size_of` / `align_of`.
    type DspState: Send + 'static;

    /// Whether the hot-reload shell may preserve live DSP state across a
    /// code-only reload. Default `true` (best-effort layout probe).
    /// Override to `false` on a state that must always re-init on reload.
    const PRESERVE_DSP_STATE: bool = true;

    #[must_use]
    fn supports_in_place() -> bool {
        false
    }

    #[must_use]
    fn bus_layouts() -> Vec<BusLayout> {
        vec![BusLayout::stereo()]
    }

    /// Build initial state from params. See [`PluginLogic::init`].
    fn init(params: &Self::Params) -> Self::DspState;

    fn reset(state: &mut Self::DspState, params: &Self::Params, config: &AudioConfig);

    fn process(
        state: &mut Self::DspState,
        params: &Self::Params,
        buffer: &mut AudioBuffer<S>,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus;

    fn save_state(state: &Self::DspState) -> Vec<u8> {
        let _ = state;
        Vec::new()
    }
    /// Lock-free state-save opt-in. See [`PluginLogic::snapshot_into`].
    fn snapshot_into(state: &Self::DspState, buf: &mut Vec<u8>) -> bool {
        let _ = (state, buf);
        false
    }
    /// Restore plugin-specific state. See [`PluginLogic::load_state`].
    ///
    /// # Errors
    ///
    /// Forwards whatever the user impl returns - typically a malformed
    /// blob error decoded by `bincode` / `serde` / similar.
    fn load_state(state: &mut Self::DspState, data: &[u8]) -> Result<(), StateLoadError> {
        let _ = (state, data);
        Ok(())
    }
    /// Translate foreign state into truce shape. See
    /// [`PluginLogic::migrate_state`].
    #[must_use]
    fn migrate_state(foreign: &ForeignState) -> Option<MigratedState> {
        let _ = foreign;
        None
    }
    fn state_changed(state: &mut Self::DspState, params: &Self::Params) {
        let _ = (state, params);
    }
    fn latency(state: &Self::DspState) -> u32 {
        let _ = state;
        0
    }
    fn tail(state: &Self::DspState) -> u32 {
        let _ = state;
        0
    }
}

/// Precision-keyed editor factory, bridged from the leaf traits.
///
/// `plugin!` / `export_static!` / `export_plugin!` build the editor from
/// the concrete logic type without naming which leaf trait
/// ([`PluginLogic`] vs [`PluginLogic64`]) it implements. Keyed on `S`
/// only so the two per-leaf blanket impls don't overlap - the editor and
/// param store are precision-independent.
///
/// This lives off [`PluginLogicCore`] on purpose: it carries an
/// associated `Params` type and a receiverless `editor`, either of which
/// would make `PluginLogicCore` non-object-safe and break the
/// hot-reload loader's type-erased `Box<dyn PluginLogicCore<S>>`. Only
/// concrete code (the shells' macros) ever names it, never `dyn`.
pub trait PluginEditor<S: Sample> {
    /// The plugin's parameter struct; mirrors the leaf's `Params`.
    type Params: truce_params::Params;

    /// Build the editor from the lock-free param store. Receiverless, so
    /// the wrapper constructs it while the audio thread runs, without the
    /// plugin lock.
    fn editor(params: std::sync::Arc<Self::Params>) -> Box<dyn Editor>;
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
        $vis trait $name: 'static {
            /// The plugin's parameter struct (`#[derive(Params)]`).
            /// Shared, immutable during a block - it arrives by
            /// reference every call from the shell, which owns the
            /// `Arc`. Never stored in [`Self::DspState`].
            type Params: $crate::__plugin_logic_deps::Params;

            /// The mutable per-block audio state - filter memory, voice
            /// buffers, phase accumulators. **A distinct type from the
            /// descriptor `Self`, never `Self`.** A plugin with no audio
            /// state writes `type DspState = ()`; anything else is a plain
            /// struct. Owned by the shell, so it can outlive a code swap.
            /// No layout trait is required - the hot-reload shell
            /// fingerprints the type at load time from its `type_name` /
            /// `size_of` / `align_of`.
            type DspState: Send + 'static;

            /// Whether the hot-reload shell may preserve live DSP state
            /// across a code-only reload. Default `true`: the shell keeps
            /// the state when a best-effort layout probe (`type_name` +
            /// `size_of` + `align_of`) matches, so a reverb tail survives
            /// an edit-and-reload, and re-inits when it differs. Set to
            /// `false` on a state that must always re-init on reload.
            const PRESERVE_DSP_STATE: bool = true;

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
            fn supports_in_place() -> bool {
                false
            }

            /// Supported audio bus configurations. The host picks one;
            /// the others are rejected at bus-config time before
            /// `process` is ever called. Default: stereo in, stereo out.
            #[must_use]
            fn bus_layouts() -> Vec<$crate::__plugin_logic_deps::BusLayout> {
                vec![$crate::__plugin_logic_deps::BusLayout::stereo()]
            }

            /// Build the initial audio state from params. Replaces the
            /// old `new` constructor: the descriptor is stateless, so
            /// state is born here and owned by the shell. Not real-time
            /// safe - allocate freely.
            fn init(params: &Self::Params) -> Self::DspState;

            /// Reset for a new sample rate / block size / processing
            /// mode. Clear `state`'s filters / delay lines; read
            /// `config.process_mode` to size buffers for an offline
            /// render (allocation belongs here, off the audio thread) -
            /// see [`AudioConfig`](truce_core::config::AudioConfig).
            ///
            /// Params plumbing is NOT your job: the shell calls
            /// `params.set_sample_rate(config.sample_rate)` and
            /// `params.snap_smoothers()` before invoking this, so the
            /// body only handles state the plugin itself owns. Default:
            /// no-op, right for a stateless plugin (`DspState = ()`).
            fn reset(
                state: &mut Self::DspState,
                params: &Self::Params,
                config: &$crate::__plugin_logic_deps::AudioConfig,
            ) {
                let _ = (state, params, config);
            }

            /// Process one block of audio. Real-time - no allocations,
            /// locks, or I/O. `state` is exclusively owned this block;
            /// `params` is shared and immutable.
            fn process(
                state: &mut Self::DspState,
                params: &Self::Params,
                buffer: &mut $crate::__plugin_logic_deps::AudioBuffer<$sample>,
                events: &$crate::__plugin_logic_deps::EventList,
                context: &mut $crate::__plugin_logic_deps::ProcessContext,
            ) -> $crate::__plugin_logic_deps::ProcessStatus;

            /// Serialize plugin-specific state (DSP state, not params -
            /// those are saved automatically). Default: delegates to
            /// [`Self::snapshot_into`] (empty when neither is
            /// overridden).
            ///
            /// Runs on a host or GUI thread while the audio thread is
            /// paused at a block boundary (the wrapper's plugin lock),
            /// so reading any field is safe - but an audio block that
            /// arrives mid-save waits for this to return. Keep it
            /// cheap: copy bytes out, don't compute or compress here.
            /// To take this off the plugin lock entirely, override
            /// [`Self::snapshot_into`] instead.
            fn save_state(state: &Self::DspState) -> Vec<u8> {
                let mut buf = Vec::new();
                let _ = Self::snapshot_into(state, &mut buf);
                buf
            }

            /// Opt into lock-free state save. `buf` arrives **cleared**,
            /// with its capacity retained across calls so a steady state
            /// is allocation-free; fill it with the same bytes
            /// [`Self::save_state`] would produce (append freely - it is
            /// never carried over from the previous block).
            ///
            /// The return value is a *static capability*, not a
            /// per-block flag: `true` means "this plugin publishes
            /// snapshots", `false` means "it never does" (the default).
            /// Once you return `true` you must return `true` for the
            /// plugin's whole lifetime - if the custom state empties out,
            /// return `true` with `buf` left empty (an empty blob), don't
            /// return `false`. The shell latches the opt-in on the first
            /// published block; a later `false` is a contract violation
            /// that would otherwise leave the host reading a stale
            /// snapshot forever.
            ///
            /// Called on the **audio thread** after each process block,
            /// under the same real-time rules as `process` - bounded, no
            /// unbounded allocation. The wrapper publishes the result
            /// into a lock-free slot the host reads without ever taking
            /// the plugin lock, so saving state while audio runs never
            /// stalls the audio thread. Overriding this is the
            /// preferred way to serialize custom state; the default
            /// [`Self::save_state`] delegates here.
            fn snapshot_into(state: &Self::DspState, buf: &mut Vec<u8>) -> bool {
                let _ = (state, buf);
                false
            }

            /// Restore plugin-specific state into `state`.
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
                state: &mut Self::DspState,
                data: &[u8],
            ) -> Result<(), $crate::__plugin_logic_deps::StateLoadError> {
                let _ = (state, data);
                Ok(())
            }

            /// Called on the audio thread immediately after
            /// [`Self::load_state`] returns. Invalidate or recompute any
            /// caches in `state` that the next `process()` reads. Default:
            /// no-op.
            fn state_changed(state: &mut Self::DspState, params: &Self::Params) {
                let _ = (state, params);
            }

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
                foreign: &$crate::__plugin_logic_deps::ForeignState,
            ) -> Option<$crate::__plugin_logic_deps::MigratedState> {
                let _ = foreign;
                None
            }

            /// Report latency in samples for plugin delay compensation.
            /// May change at runtime - return a new value and the host is
            /// notified (see the wrapper latency-change path).
            fn latency(state: &Self::DspState) -> u32 {
                let _ = state;
                0
            }

            /// Report tail time in samples (audio produced after input
            /// stops - reverbs, delays). `u32::MAX` for infinite tail.
            fn tail(state: &Self::DspState) -> u32 {
                let _ = state;
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
            ///
            /// An associated function, not a method: it receives the
            /// lock-free `Arc<Self::Params>` store the wrapper already
            /// holds, so the host can open the editor while audio is
            /// running without ever taking the plugin lock. Editors bind
            /// only to the param store (plus meters / transport, all
            /// lock-free); custom DSP state is read at runtime through
            /// the editor bridge, not at construction.
            fn editor(
                params: ::std::sync::Arc<Self::Params>,
            ) -> Box<dyn $crate::__plugin_logic_deps::Editor>;
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
    pub use truce_core::config::AudioConfig;
    pub use truce_core::dsp_state::{NO_PRESERVE, layout_fingerprint};
    pub use truce_core::editor::Editor;
    pub use truce_core::events::EventList;
    pub use truce_core::process::{ProcessContext, ProcessStatus};
    pub use truce_core::state::{ForeignState, MigratedState, StateLoadError};
    pub use truce_params::Params;
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
    /// `truce_gui::default_editor(params, layout())` (where `layout()`
    /// is a plain inherent method on the plugin struct, not part of the
    /// trait).
    ///
    /// ## Params vs. DSP state
    ///
    /// The struct you implement this on holds two different kinds of
    /// data, and the method receivers reflect the split:
    ///
    /// - **Params** - the user-facing values in your `#[derive(Params)]`
    ///   struct, held as `Arc<Self::Params>`. Atomic-backed and `Sync`,
    ///   shared lock-free with the host and the editor.
    /// - **DSP state** - everything else on the struct: filter memory,
    ///   phase accumulators, voice buffers, delay lines. Plain and
    ///   non-atomic, mutated every sample, exclusive to the audio thread.
    ///
    /// `process` / `reset` / `load_state` take `&mut self` because they
    /// mutate DSP state; `save_state` / `snapshot_into` take `&self`
    /// because they read it. `editor` takes neither - it is an
    /// associated function over the param store, because a GUI is a
    /// *view* that binds only params (plus lock-free meters / transport)
    /// and never touches DSP state, so it can be built without the
    /// plugin lock. DSP state can't move into params: making per-sample
    /// filter memory atomic-shared would put a synchronized access on
    /// the hottest path, and it isn't a "parameter" anyway.
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
            type Params = <T as $leaf>::Params;
            type DspState = <T as $leaf>::DspState;

            const PRESERVE_DSP_STATE: bool = <T as $leaf>::PRESERVE_DSP_STATE;

            fn supports_in_place() -> bool {
                <Self as $leaf>::supports_in_place()
            }

            fn bus_layouts() -> Vec<BusLayout> {
                <Self as $leaf>::bus_layouts()
            }

            fn init(params: &Self::Params) -> Self::DspState {
                <Self as $leaf>::init(params)
            }

            fn reset(state: &mut Self::DspState, params: &Self::Params, config: &AudioConfig) {
                <Self as $leaf>::reset(state, params, config);
            }

            fn process(
                state: &mut Self::DspState,
                params: &Self::Params,
                buffer: &mut AudioBuffer<$sample>,
                events: &EventList,
                context: &mut ProcessContext,
            ) -> ProcessStatus {
                // FTZ/DAZ (or FZ on AArch64) for the duration of
                // the user's process body. Denormals on filter
                // feedback paths stall the core; the guard pays
                // ~two MXCSR writes per block to avoid that. Both the
                // static and hot shells route process through here, so
                // this brackets exactly the user body in both modes.
                let _denormal_guard = DenormalGuard::new();
                <Self as $leaf>::process(state, params, buffer, events, context)
            }

            fn save_state(state: &Self::DspState) -> Vec<u8> {
                <Self as $leaf>::save_state(state)
            }

            fn snapshot_into(state: &Self::DspState, buf: &mut Vec<u8>) -> bool {
                <Self as $leaf>::snapshot_into(state, buf)
            }

            fn load_state(state: &mut Self::DspState, data: &[u8]) -> Result<(), StateLoadError> {
                <Self as $leaf>::load_state(state, data)
            }

            fn state_changed(state: &mut Self::DspState, params: &Self::Params) {
                <Self as $leaf>::state_changed(state, params);
            }

            fn migrate_state(foreign: &ForeignState) -> Option<MigratedState> {
                <Self as $leaf>::migrate_state(foreign)
            }

            fn latency(state: &Self::DspState) -> u32 {
                <Self as $leaf>::latency(state)
            }

            fn tail(state: &Self::DspState) -> u32 {
                <Self as $leaf>::tail(state)
            }
        }

        impl<T: $leaf> PluginEditor<$sample> for T {
            type Params = <T as $leaf>::Params;

            fn editor(params: std::sync::Arc<Self::Params>) -> Box<dyn Editor> {
                <Self as $leaf>::editor(params)
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
