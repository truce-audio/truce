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
//! Plus one layer of sugar: [`PurePluginLogic`] / [`PurePluginLogic64`]
//! for plugins with no DSP state, blanket-implemented into the
//! matching leaf so everything downstream sees a normal `PluginLogic`
//! with `DspState = ()`.
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

    /// Supported audio bus configurations. The host picks one. Default:
    /// the standard audio effect - stereo and mono - so it appears on
    /// both track widths. Override for instruments, surround, sidechains.
    #[must_use]
    fn bus_layouts() -> Vec<BusLayout> {
        BusLayout::stereo_and_mono()
    }

    /// Build initial state from params. See [`PluginLogic::init`].
    fn init(params: &Self::Params, cx: &InitContext) -> Self::DspState;

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
            /// buffers, phase accumulators. A plain struct, distinct from
            /// the descriptor `Self` (except for the small-effect
            /// `type DspState = Self` shape). A plugin with no audio state
            /// implements the stateless leaf trait instead of this one and
            /// never names a `DspState`. Owned by the shell, so it can
            /// outlive a code swap.
            /// `Default` is how the state is born: the default
            /// [`Self::init`] returns `Self::DspState::default()`, so most
            /// plugins never write `init` - they `#[derive(Default)]` (or
            /// hand-write `Default` when a fresh state has non-zero fields)
            /// and override `init` only when construction needs params.
            /// No layout trait is required - the hot-reload shell
            /// fingerprints the type at load time from its `type_name` /
            /// `size_of` / `align_of`.
            type DspState: ::core::default::Default + Send + 'static;

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
            /// `process` is ever called. Default: the standard audio
            /// effect - stereo and mono - so it shows on both track widths.
            #[must_use]
            fn bus_layouts() -> Vec<$crate::__plugin_logic_deps::BusLayout> {
                $crate::__plugin_logic_deps::BusLayout::stereo_and_mono()
            }

            /// Build the initial audio state from params. Replaces the
            /// old `new` constructor: the descriptor is stateless, so
            /// state is born here and owned by the shell. Not real-time
            /// safe - allocate freely.
            ///
            /// Default: `Self::DspState::default()`. Override only when
            /// construction genuinely needs to read params; a fixed
            /// initial state belongs in the state type's `Default` impl
            /// instead.
            fn init(
                params: &Self::Params,
                cx: &$crate::__plugin_logic_deps::InitContext,
            ) -> Self::DspState {
                let _ = (params, cx);
                ::core::default::Default::default()
            }

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
    pub use truce_core::tasks::{InitContext, TaskSpawner};
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
    /// Required: [`Self::process`], [`Self::editor`]. Everything else
    /// has a default: `init` builds `Self::DspState::default()` unless
    /// construction needs params, and `reset` is a no-op. The editor is
    /// constructed explicitly - layout-only plugins typically call
    /// `truce_gui::default_editor(params, layout())` (where `layout()`
    /// is a plain inherent method on the plugin struct, not part of the
    /// trait). A plugin with no DSP state at all should implement
    /// [`PurePluginLogic`] instead and skip the state plumbing entirely.
    ///
    /// ## Params vs. DSP state
    ///
    /// The type you implement this on is a stateless descriptor; the
    /// data lives in two places, and the method signatures reflect the
    /// split:
    ///
    /// - **Params** (`type Params`) - the user-facing values in your
    ///   `#[derive(Params)]` struct, held as `Arc<Self::Params>`.
    ///   Atomic-backed and `Sync`, shared lock-free with the host and
    ///   the editor. Arrives read-only as `&Self::Params`.
    /// - **DSP state** (`type DspState`) - filter memory, phase
    ///   accumulators, voice buffers, delay lines. Plain and
    ///   non-atomic, mutated every sample, exclusive to the audio
    ///   thread. Owned by the shell, passed `&mut` to the methods that
    ///   mutate it (`process` / `reset` / `load_state`) and `&` to the
    ///   ones that read it (`save_state` / `snapshot_into`).
    ///
    /// `editor` takes neither - it is an associated function over the
    /// param store, because a GUI is a *view* that binds only params
    /// (plus lock-free meters / transport) and never touches DSP state,
    /// so it can be built without the plugin lock. DSP state can't move
    /// into params: making per-sample filter memory atomic-shared would
    /// put a synchronized access on the hottest path, and it isn't a
    /// "parameter" anyway.
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
// Pure leaf traits - stateless sugar over PluginLogic / PluginLogic64
// ---------------------------------------------------------------------------

/// Define a sample-pinned pure (stateless) leaf trait plus its blanket
/// impl into the matching stateful leaf. Two invocations: `PurePluginLogic`
/// over [`PluginLogic`] and [`PurePluginLogic64`] over [`PluginLogic64`].
/// A macro for the same reason as [`plugin_logic_leaf_trait!`]: the two
/// surfaces stay in lock-step by construction.
macro_rules! pure_plugin_leaf_trait {
    ($(#[$attr:meta])* $vis:vis trait $name:ident: $leaf:ident<sample = $sample:ty>) => {
        $(#[$attr])*
        $vis trait $name: 'static {
            /// The plugin's parameter struct (`#[derive(Params)]`).
            /// Shared, immutable during a block - it arrives by
            /// reference every call from the shell, which owns the
            /// `Arc`.
            type Params: crate::__plugin_logic_deps::Params;

            /// Opt into zero-copy in-place I/O. Same contract as the
            /// stateful leaf's `supports_in_place`.
            #[must_use]
            fn supports_in_place() -> bool {
                false
            }

            /// Supported audio bus configurations. Same contract as the
            /// stateful leaf's `bus_layouts`. Default: the standard audio
            /// effect - stereo and mono.
            #[must_use]
            fn bus_layouts() -> Vec<crate::__plugin_logic_deps::BusLayout> {
                crate::__plugin_logic_deps::BusLayout::stereo_and_mono()
            }

            /// Process one block of audio as a pure function of params
            /// and input. Same real-time contract as the stateful
            /// leaf's `process`, minus the state argument.
            fn process(
                params: &Self::Params,
                buffer: &mut crate::__plugin_logic_deps::AudioBuffer<$sample>,
                events: &crate::__plugin_logic_deps::EventList,
                context: &mut crate::__plugin_logic_deps::ProcessContext,
            ) -> crate::__plugin_logic_deps::ProcessStatus;

            /// Translate foreign state into truce shape. Same contract
            /// as the stateful leaf's `migrate_state` - a stateless
            /// plugin may still inherit params from a previous
            /// framework's blob.
            #[must_use]
            fn migrate_state(
                foreign: &crate::__plugin_logic_deps::ForeignState,
            ) -> Option<crate::__plugin_logic_deps::MigratedState> {
                let _ = foreign;
                None
            }

            /// Construct the editor for this plugin. Required. Same
            /// contract as the stateful leaf's `editor`.
            fn editor(
                params: ::std::sync::Arc<Self::Params>,
            ) -> Box<dyn crate::__plugin_logic_deps::Editor>;
        }

        // The blanket that makes the sugar real: a pure plugin IS a
        // stateful plugin with `DspState = ()`. Everything downstream
        // (`PluginLogicCore`, `PluginEditor`, the shells, `plugin!`)
        // binds through $leaf and never learns the difference. Methods
        // not forwarded here (`init`, `reset`, `save_state`, `latency`,
        // `tail`, ...) keep their $leaf defaults, which are exactly the
        // stateless behaviors.
        impl<T: $name> $leaf for T {
            type Params = <T as $name>::Params;
            type DspState = ();

            fn supports_in_place() -> bool {
                <T as $name>::supports_in_place()
            }

            fn bus_layouts() -> Vec<crate::__plugin_logic_deps::BusLayout> {
                <T as $name>::bus_layouts()
            }

            fn process(
                _state: &mut (),
                params: &Self::Params,
                buffer: &mut crate::__plugin_logic_deps::AudioBuffer<$sample>,
                events: &crate::__plugin_logic_deps::EventList,
                context: &mut crate::__plugin_logic_deps::ProcessContext,
            ) -> crate::__plugin_logic_deps::ProcessStatus {
                <T as $name>::process(params, buffer, events, context)
            }

            fn migrate_state(
                foreign: &crate::__plugin_logic_deps::ForeignState,
            ) -> Option<crate::__plugin_logic_deps::MigratedState> {
                <T as $name>::migrate_state(foreign)
            }

            fn editor(
                params: ::std::sync::Arc<Self::Params>,
            ) -> Box<dyn crate::__plugin_logic_deps::Editor> {
                <T as $name>::editor(params)
            }
        }
    };
}

pure_plugin_leaf_trait! {
    /// The stateless `f32` plugin trait: [`PluginLogic`] minus every
    /// state-shaped item. For a pure parameter-driven effect - one
    /// whose `process` is a function of params and input only - this
    /// removes the `type DspState = ()` / `init` / `_state: &mut ()`
    /// plumbing entirely:
    ///
    /// ```ignore
    /// pub struct Gain;
    ///
    /// impl PurePluginLogic for Gain {
    ///     type Params = GainParams;
    ///     fn process(params: &GainParams, buffer: &mut AudioBuffer, events: &EventList, ctx: &mut ProcessContext) -> ProcessStatus {
    ///         /* ... */
    ///     }
    ///     fn editor(params: Arc<GainParams>) -> Box<dyn Editor> { /* ... */ }
    /// }
    /// ```
    ///
    /// A blanket impl makes every `PurePluginLogic` a [`PluginLogic`] with
    /// `DspState = ()`, so `truce::plugin!` and every shell consume it
    /// unchanged - and implementing both traits for one type is
    /// correctly rejected as conflicting. When the plugin grows DSP
    /// state, switch the impl header to [`PluginLogic`] and add the
    /// state type and arguments.
    ///
    /// Required: [`Self::process`], [`Self::editor`]. Optional:
    /// [`Self::bus_layouts`], [`Self::supports_in_place`],
    /// [`Self::migrate_state`]. Anything state-shaped (`reset`,
    /// `save_state`, `latency`, `tail`, ...) needs state to act on -
    /// implement [`PluginLogic`] directly if you need those.
    pub trait PurePluginLogic: PluginLogic<sample = f32>
}

pure_plugin_leaf_trait! {
    /// The stateless `f64` plugin trait. Same surface as
    /// [`PurePluginLogic`] but with the audio buffer pinned to `f64`;
    /// blanket-implements [`PluginLogic64`]. `truce::prelude64`
    /// re-exports it as `PurePluginLogic`, so the impl header reads the
    /// same regardless of precision.
    pub trait PurePluginLogic64: PluginLogic64<sample = f64>
}

// ---------------------------------------------------------------------------
// Background tasks - opt-in managed off-thread work
// ---------------------------------------------------------------------------

pub use crate::__plugin_logic_deps::{InitContext, TaskSpawner};

/// Opt-in managed background work. Each **task type** implements this to
/// declare its params, its concurrency mode, and the handler the framework
/// runs on a shared background-thread pool; a plugin lists one or more task
/// types with the `tasks:` key on `truce::plugin!` (`tasks: [Rebuild,
/// Analyze]`). Every task type gets its own inbound queue and mode, so a
/// serialized lane and a concurrent lane coexist in one plugin. Nothing
/// changes for a plugin that declares no tasks.
///
/// `run` executes off the audio thread and reaches shared state through
/// `params` (its `#[skip]` channels / atomics), exactly like the editor:
/// it must never touch `DspState`, which is audio-thread-exclusive.
/// Feedback to the audio thread stays the plugin's job through those
/// `#[skip]` channels.
///
/// Keep handlers short and non-blocking. The pool is shared by every
/// truce plugin in the host and small (`available_parallelism() - 1`
/// threads, as few as one), so a handler that blocks on I/O (reading a
/// sample off disk) or waits on a lock stalls background work for *every
/// other instance too*, not just its own. Allocation and CPU-bound bursts
/// are fine - that is what the pool is for. For work that genuinely blocks
/// or runs long, give the plugin its own thread with
/// `AudioTap::spawn_worker` rather than the shared pool.
///
/// Schedule tasks with `ctx.tasks::<Rebuild>()` from `process` (wait-free),
/// the editor's `PluginContext`, or the `InitContext` passed to `init` -
/// the type parameter selects the lane.
///
/// ```ignore
/// struct Rebuild { sample_rate: f64, time_s: f32 }
/// impl BackgroundTask for Rebuild {
///     type Params = ReverbParams;
///     const SERIALIZED: bool = true;   // non-reentrant graph build
///     fn run(self, params: &ReverbParams) {
///         let graph = build_graph(self.sample_rate, self.time_s);
///         let _ = params.ready.force_push(graph);   // #[skip] handoff
///     }
/// }
/// // truce::plugin! { logic, params, tasks: [Rebuild] }
/// ```
pub trait BackgroundTask: Send + 'static {
    /// The plugin's parameter struct; must match the leaf trait's
    /// `type Params`. (`Send`/`'static` on the task type itself: the pool
    /// moves it across threads and the worker outlives any block.)
    type Params: crate::__plugin_logic_deps::Params;
    /// Run this lane's handler one at a time for a given instance
    /// ("one-slot" mode).
    ///
    /// Default `false`: the pool is shared and lock-free, so a burst that
    /// re-arms a lane while a worker is still draining it can hand a second
    /// worker the same lane - `run` may run **concurrently with itself**
    /// for one instance. A handler that only talks to the audio thread
    /// through lock-free channels / atomics (the reverb example's MPMC
    /// handoff) is fine that way and keeps maximum throughput.
    ///
    /// Set `true` when the handler read-modify-writes shared mutable state
    /// that isn't safe to enter re-entrantly (a scratch buffer, a
    /// non-atomic cache): the pool then serializes this lane's drains so at
    /// most one `run` for this instance runs at a time, without the author
    /// needing a `try_lock` guard. Tasks are never dropped or reordered;
    /// serialization only bounds concurrency, so keep the handler short
    /// (a long serialized handler delays this lane's later tasks). The mode
    /// is per lane, so a concurrent lane in the same plugin is unaffected.
    const SERIALIZED: bool = false;
    /// Run one task on the pool. See the trait docs for the contract,
    /// including the concurrency note on [`Self::SERIALIZED`].
    fn run(self, params: &Self::Params);
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

            fn init(
                params: &Self::Params,
                cx: &crate::__plugin_logic_deps::InitContext,
            ) -> Self::DspState {
                <Self as $leaf>::init(params, cx)
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
