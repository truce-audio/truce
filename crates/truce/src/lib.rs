#![forbid(unsafe_code)]

pub use truce_core as core;
pub use truce_derive::{ParamEnum, Params, State};
pub use truce_gui as gui;
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

    #[cfg(feature = "shell")]
    pub use truce_loader::shell::HotShell;

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

// Single implementation module; the three preludes are wafer-thin
// alias wrappers that swap the `FloatParamRead*` trait + `Sample`
// type alias.
mod prelude_impl {
    pub use std::f64::consts::TAU;
    pub use std::sync::Arc;
    pub use truce_core::custom_state::{State as StateTrait, StateBinding, StateField};
    pub use truce_core::sample::{Float, Sample as SampleTrait};
    pub use truce_core::state::StateLoadError;
    pub use truce_core::util::{db_to_linear, linear_to_db, meter_display, midi_note_to_freq};
    pub use truce_core::{
        AudioBuffer, BusConfig, BusKind, BusLayout, ChannelConfig, Editor, Event, EventBody,
        EventList, Plugin, PluginCategory, PluginContext, PluginExport, PluginInfo, ProcessContext,
        ProcessStatus, TransportInfo,
    };
    pub use truce_derive::{ParamEnum, Params, State, plugin_info};
    pub use truce_gui::PluginLogic;
    pub use truce_gui::interaction::WidgetRegion;
    pub use truce_gui::render::RenderBackend;
    pub use truce_gui::theme::{Color, Theme};
    pub use truce_params::{
        BoolParam, EnumParam, FloatParam, IntParam, MeterSlot, ParamEnum, ParamFlags, ParamInfo,
        ParamRange, ParamUnit, Params, Smoother, SmoothingStyle,
    };
}

/// `f32`-flavoured prelude. Re-exports every symbol from [`prelude`]
/// plus the [`FloatParamReadF32`](truce_params::FloatParamReadF32)
/// extension trait (via `as _`), which makes `param.read()` resolve
/// to `f32` without per-call annotation.
///
/// Plugin DSP under this prelude writes:
///
/// ```ignore
/// use truce::prelude32::*;
/// let gain = self.params.gain.read();   // f32 — unambiguous
/// out[i] = inp[i] * gain;
/// ```
///
/// `truce::prelude` and `truce::prelude32` are interchangeable —
/// pick whichever reads better at the use site. Mirrors fundsp's
/// `prelude` / `prelude32`.
pub mod prelude32 {
    pub use super::prelude_impl::*;
    pub use truce_params::FloatParamReadF32 as _;
    /// Audio sample type for this prelude.
    pub type Sample = f32;
}

/// `f64`-flavoured prelude. Re-exports every symbol from [`prelude`]
/// plus the [`FloatParamReadF64`](truce_params::FloatParamReadF64)
/// extension trait. Use this when the audio path is `f64` end-to-end
/// (high-order biquads, oscillator phase accumulators, long-running
/// cumulative state where 24-bit f32 precision shows up audibly).
///
/// The format wrapper widens the host's audio buffer to `f64` at
/// the block boundary and narrows on the way out. Pure-`f32`
/// plugins under `prelude32` keep the zero-copy fast path. Mixed
/// precision (per-value `to_f32` / `to_f64`) is fully supported
/// under either prelude.
///
/// **Don't import both `prelude` and `prelude64` in the same file**
/// — the two `read` / `value` / `current` traits will collide on
/// method dispatch. That collision is the right error if the file
/// hasn't committed to a precision.
pub mod prelude64 {
    pub use super::prelude_impl::*;
    pub use truce_params::FloatParamReadF64 as _;
    /// Audio sample type for this prelude.
    pub type Sample = f64;
}

/// Mixed-precision prelude (`m` for "mixed"). The audio buffer
/// stays at host wire precision (`f32` — no wrapper-boundary widening
/// cost) but `param.read()` returns `f64` so intermediary math
/// (filter coefficients, phase accumulators, long-tail feedback)
/// runs at `f64` precision.
///
/// Plugin DSP under this prelude writes the narrowing cast at the
/// buffer-write site:
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
/// Trade vs [`prelude64`]: you skip the wrapper's per-block widen +
/// narrow memcpy at the cost of writing `.to_f32()` on the way out.
/// Pick this when the wrapper boundary cost actually shows up in
/// the profiler (very high channel counts, very small blocks);
/// otherwise [`prelude64`] is the cleaner choice.
pub mod prelude64m {
    pub use super::prelude_impl::*;
    pub use truce_params::FloatParamReadF64 as _;
    /// Audio sample type for this prelude — `f32` (host wire),
    /// despite param reads being `f64`.
    pub type Sample = f32;
}

/// Default prelude. Alias for [`prelude32`] — `f32` audio path. Use
/// whichever name reads better at the import site.
pub mod prelude {
    pub use super::prelude_impl::*;
    pub use truce_params::FloatParamReadF32 as _;
    /// Audio sample type for this prelude.
    pub type Sample = f32;
}
