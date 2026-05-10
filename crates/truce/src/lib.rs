#![forbid(unsafe_code)]

pub use truce_core as core;
pub use truce_derive::{ParamEnum, Params, State};
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

/// Prelude — import everything a plugin author needs.
pub mod prelude {
    pub use std::f64::consts::TAU;
    pub use std::sync::Arc;
    pub use truce_core::custom_state::{State as StateTrait, StateBinding, StateField};
    pub use truce_core::util::{db_to_linear, linear_to_db, meter_display, midi_note_to_freq};
    pub use truce_core::{
        AudioBuffer, BusConfig, BusKind, BusLayout, ChannelConfig, Editor, Event, EventBody,
        EventList, Plugin, PluginCategory, PluginContext, PluginExport, PluginInfo, PluginLogic,
        ProcessContext, ProcessStatus, TransportInfo,
    };
    pub use truce_derive::{ParamEnum, Params, State, plugin_info};
    pub use truce_gui::PluginEditor;
    pub use truce_gui::interaction::WidgetRegion;
    pub use truce_gui::render::RenderBackend;
    pub use truce_gui::theme::{Color, Theme};
    pub use truce_params::{
        BoolParam, EnumParam, FloatParam, IntParam, MeterSlot, ParamEnum, ParamFlags, ParamInfo,
        ParamRange, ParamUnit, Params, Smoother, SmoothingStyle,
    };
}
