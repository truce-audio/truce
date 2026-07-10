#![forbid(unsafe_code)]

pub use truce_core as core;
/// Real-time allocation checking helpers (`allow_alloc`, `audit`). The
/// checker itself is the opt-in `rt-paranoid` feature; install it with
/// [`enable_rt_paranoid!`]. See the audio-testing guide.
pub use truce_core::rt;
pub use truce_derive::{ParamEnum, Params, State};
// `truce` is renderer-agnostic - it no longer re-exports `truce-gui`.
// Plugins pick a renderer crate (truce-gui, truce-egui, truce-iced,
// truce-slint) directly in their Cargo.toml and use it inside their
// `PluginLogic::editor()` impl. The prelude below sources GUI types
// from the lightweight `truce-gui-types` so layout / interaction /
// theme remain available without dragging tiny-skia + baseview + wgpu.
pub use truce_params as params;

#[cfg(feature = "clap")]
pub use truce_clap as clap_wrapper;

#[cfg(feature = "vst3")]
pub use truce_vst3 as vst3_wrapper;

mod plugin_macro;

/// Re-exports used by the plugin! macro internals.
#[doc(hidden)]
pub mod __reexport {
    pub use truce_derive::__truce_lv2_emit_root;
    pub use truce_loader::{export_plugin, export_static};
    pub use truce_plugin;

    #[cfg(feature = "shell")]
    pub use truce_loader::shell::HotShell;

    #[cfg(feature = "rt-paranoid")]
    pub use truce_core::rt::RtCheckAlloc;

    /// Hot-reload sidecar path resolver. Routed through
    /// `truce_core::shell_sidecar` so plugin crates that expand
    /// `truce::plugin!` only need `truce` in their dependency set;
    /// the `#[cfg(feature = "shell")]` arm calls this at runtime.
    #[cfg(feature = "shell")]
    #[must_use]
    pub fn shell_sidecar_path(crate_name: &str) -> Option<std::path::PathBuf> {
        truce_core::shell_sidecar::sidecar_path(crate_name)
    }
}

/// Install the paranoid real-time allocation checker's global allocator.
///
/// Call once at your crate (or test binary) root:
///
/// ```ignore
/// truce::enable_rt_paranoid!();
/// ```
///
/// The call is unconditional; the `truce/rt-paranoid` feature does the
/// gating. With the feature off this expands to nothing (no allocator is
/// installed, so it never collides with a custom `#[global_allocator]`),
/// and the check in `process` compiles away. With it on, allocations the
/// audio thread makes inside `process` are flagged. `truce::rt::set_mode`
/// selects the reaction: `Count` (default, log after the block), `Panic`
/// (fail the block), `Trap` (abort at the allocation). The `truce-test`
/// `assert_no_audio_alloc` helper gates a test regardless of the mode.
///
/// A global allocator must be declared in the final artifact, which a
/// library cannot do for you - hence a macro you place, rather than
/// automatic emission from `truce::plugin!` (which would also clash with
/// any allocator you set yourself).
#[cfg(feature = "rt-paranoid")]
#[macro_export]
macro_rules! enable_rt_paranoid {
    () => {
        #[global_allocator]
        static __TRUCE_RT_ALLOC: $crate::__reexport::RtCheckAlloc =
            $crate::__reexport::RtCheckAlloc::new();
    };
}

/// No-op form: the `rt-paranoid` feature is off, so installing the
/// checking allocator would be wrong (it must not replace a plugin's own
/// `#[global_allocator]`). See the feature-on variant for docs.
#[cfg(not(feature = "rt-paranoid"))]
#[macro_export]
macro_rules! enable_rt_paranoid {
    () => {};
}

// Single implementation module; the four preludes are wafer-thin
// alias wrappers that swap the `FloatParamRead*` trait + `Sample`
// type alias.
mod prelude_impl {
    pub use std::f64::consts::TAU;
    pub use std::sync::Arc;
    pub use truce_core::custom_state::{AtomicCell, State as StateTrait, StateBinding, StateField};
    pub use truce_core::sample::{Float, Sample as SampleTrait};
    pub use truce_core::state::{ForeignState, MigratedState, PluginFormat, StateLoadError};
    // Managed background tasks: the opt-in `BackgroundTask` trait, the
    // `InitContext` `init` receives, the `TaskSpawner` handle
    // `ctx.tasks::<T>()` returns, and the `AudioTap` audio->worker ring.
    pub use truce_core::audio_tap::{AudioTap, StreamWorker};
    pub use truce_core::tasks::{InitContext, TaskSpawner};
    pub use truce_core::util::{db_to_linear, linear_to_db, meter_display, midi_note_to_freq};
    pub use truce_plugin::BackgroundTask;
    // `AudioBuffer` is *not* re-exported from `truce_core` here -
    // each prelude module declares its own per-precision
    // `pub type AudioBuffer<'a> = truce_core::buffer::AudioBuffer<'a, $sample>;`
    // so the user impl sees the right buffer precision through scope
    // resolution.
    // `PluginRuntime` (the format-wrapper-facing trait, formerly
    // `truce_core::Plugin`) is intentionally NOT re-exported here.
    // It's implemented by the macro-generated wrapper, never by
    // plugin authors; keeping it out of the prelude prevents the
    // name collision with the new user-facing `Plugin` trait and
    // signals it as an internal contract.
    pub use truce_core::{
        AudioConfig, BusConfig, BusKind, BusLayout, ChannelConfig, Editor, Event, EventBody,
        EventList, IntoEditor, PluginCategory, PluginContext, PluginExport, PluginInfo,
        ProcessContext, ProcessMode, ProcessStatus, TransportInfo,
    };
    pub use truce_derive::{ParamEnum, Params, State, plugin_info};
    // `PluginLogic` itself is *not* re-exported here - each prelude
    // chooses its own leaf trait (`PluginLogic` for f32, aliased
    // `PluginLogic64 as PluginLogic` for f64) so plugin authors write
    // `impl PluginLogic for X { ... }` without naming a precision.
    // Source from `truce_gui_types` (types only, no rasterizer or
    // windowing) so the prelude doesn't drag the heavy `truce_gui`
    // crate in for plugins that just describe a layout.
    pub use truce_gui_types::interaction::WidgetRegion;
    pub use truce_gui_types::render::RenderBackend;
    pub use truce_gui_types::theme::{Color, Theme};
    pub use truce_params::{
        BoolParam, EnumParam, FloatParam, IntParam, MeterSlot, ParamEnum, ParamFlags, ParamInfo,
        ParamRange, ParamUnit, Params, Smoother, SmoothingStyle,
    };
}

// Stamps a prelude module from one template - all four preludes
// (default / 32 / 64 / 64m) only differ in:
//   - `$sample` - the buffer type (`f32` / `f64`)
//   - `$leaf`   - which `PluginLogic` leaf trait to alias as
//                  `PluginLogic` in the user's scope
//   - `$pure`   - which `PurePluginLogic` leaf trait to alias as
//                  `PurePluginLogic` (the stateless sugar over `$leaf`)
//   - `$float_read` / `$ctx_read` - which precision-routed read
//                  traits to bring in via `as _`
//
// Keeping the bodies in one place means a future addition (new
// extension trait, additional re-export) lands once instead of
// four times.
macro_rules! define_prelude {
    (
        $(#[$attr:meta])*
        $name:ident, sample = $sample:ty, leaf = $leaf:ident, pure = $pure:ident,
        float_read = $float_read:ident, ctx_read = $ctx_read:ident
    ) => {
        $(#[$attr])*
        pub mod $name {
            pub use super::prelude_impl::*;
            pub use truce_core::editor::$ctx_read as _;
            /// User-facing leaf trait. The prelude renames the
            /// precision-pinned leaf to `PluginLogic` so plugin
            /// authors write the same `impl PluginLogic for X { ... }`
            /// header regardless of which prelude they imported.
            // `PluginLogic` lives in `truce_plugin`; sourcing it
            // directly (rather than via the `truce_gui` re-export)
            // means the prelude doesn't pin a dep on the renderer
            // crate just to name the leaf trait.
            pub use truce_plugin::$leaf as PluginLogic;
            /// Stateless leaf trait, precision-renamed like
            /// `PluginLogic`. A plugin with no DSP state implements
            /// this instead and skips `type DspState` / `init` /
            /// the `_state` arguments entirely.
            pub use truce_plugin::$pure as PurePluginLogic;
            pub use truce_params::$float_read as _;
            /// Audio sample type for this prelude.
            pub type Sample = $sample;
            /// `AudioBuffer` with `S` defaulted to this prelude's
            /// `Sample`. The defaulted type parameter (stable since
            /// Rust 1.27) lets plugin code use the precision-pinned
            /// shorthand `&mut AudioBuffer` *and* still override it
            /// explicitly when some piece of code needs a different
            /// precision in the same file (e.g., a helper that
            /// processes both `AudioBuffer<f32>` and
            /// `AudioBuffer<f64>`). `S` only defaults when the
            /// type-arg list is empty.
            pub type AudioBuffer<'a, S = Sample> = truce_core::buffer::AudioBuffer<'a, S>;
        }
    };
}

define_prelude! {
    /// Default prelude. Same shape as [`prelude32`] - `f32` audio
    /// path. Use whichever name reads better at the import site.
    prelude, sample = f32, leaf = PluginLogic, pure = PurePluginLogic,
    float_read = FloatParamReadF32, ctx_read = PluginContextReadF32
}

define_prelude! {
    /// `f32`-flavoured prelude. `param.read()` resolves to `f32`
    /// via [`FloatParamReadF32`](truce_params::FloatParamReadF32);
    /// the audio buffer is `f32` (the host wire format for nearly
    /// every plugin format).
    prelude32, sample = f32, leaf = PluginLogic, pure = PurePluginLogic,
    float_read = FloatParamReadF32, ctx_read = PluginContextReadF32
}

define_prelude! {
    /// `f64`-flavoured prelude. The audio buffer is `f64` end-to-end
    /// (high-order biquads, oscillator phase accumulators,
    /// long-running cumulative state where 24-bit f32 precision
    /// shows up audibly). The format wrapper widens the host's
    /// audio buffer to `f64` at the block boundary and narrows on
    /// the way out.
    ///
    /// **Don't import both `prelude` and `prelude64` in the same
    /// file** - the two `read` / `value` / `current` traits will
    /// collide on method dispatch. That collision is the right
    /// error if the file hasn't committed to a precision.
    prelude64, sample = f64, leaf = PluginLogic64, pure = PurePluginLogic64,
    float_read = FloatParamReadF64, ctx_read = PluginContextReadF64
}

define_prelude! {
    /// Mixed-precision prelude (`m` for "mixed"). The audio buffer
    /// stays at host wire precision (`f32` - no wrapper-boundary
    /// widening cost) but `param.read()` returns `f64` so
    /// intermediary math (filter coefficients, phase accumulators,
    /// long-tail feedback) runs at `f64` precision.
    ///
    /// Plugin DSP under this prelude writes the narrowing cast at
    /// the buffer-write site:
    ///
    /// ```ignore
    /// use truce::prelude64m::*;
    /// use truce_core::Float; // brings `.to_f32()` into scope
    ///
    /// let cutoff = self.params.cutoff.read(); // f64
    /// let gain   = self.params.gain.read();   // f64
    /// // ... f64 math ...
    /// out[i] = (sample * gain).to_f32();      // narrow once at the edge
    /// ```
    ///
    /// Trade vs [`prelude64`]: you skip the wrapper's per-block
    /// widen + narrow memcpy at the cost of writing `.to_f32()` on
    /// the way out. Pick this when the wrapper boundary cost
    /// actually shows up in the profiler (very high channel counts,
    /// very small blocks); otherwise [`prelude64`] is the cleaner
    /// choice.
    prelude64m, sample = f32, leaf = PluginLogic, pure = PurePluginLogic,
    float_read = FloatParamReadF64, ctx_read = PluginContextReadF64
}
