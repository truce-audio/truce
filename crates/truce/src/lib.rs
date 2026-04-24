#![forbid(unsafe_code)]

pub use truce_core as core;
pub use truce_params as params;
pub use truce_params_derive::{ParamEnum, Params, State};

#[cfg(feature = "clap")]
pub use truce_clap as clap_wrapper;

#[cfg(feature = "vst3")]
pub use truce_vst3 as vst3_wrapper;

mod plugin_macro;

/// Re-exports used by the plugin! macro internals.
#[doc(hidden)]
pub mod __reexport {
    pub use truce_loader::{export_plugin, export_static};

    #[cfg(feature = "dev")]
    pub use truce_loader::shell::HotShell;
}

/// Prelude — import everything a plugin author needs.
pub mod prelude {
    pub use std::f64::consts::TAU;
    pub use std::sync::Arc;
    pub use truce_core::custom_state::{State as StateTrait, StateBinding, StateField};
    pub use truce_core::util::{db_to_linear, linear_to_db, meter_display, midi_note_to_freq};
    pub use truce_core::{
        AudioBuffer, BusConfig, BusLayout, ChannelConfig, Editor, EditorContext, Event, EventBody,
        EventList, Plugin, PluginCategory, PluginExport, PluginInfo, ProcessContext, ProcessStatus,
        TransportInfo,
    };
    pub use truce_derive::plugin_info;
    pub use truce_params::{
        BoolParam, EnumParam, FloatParam, IntParam, MeterSlot, ParamEnum, ParamFlags, ParamInfo,
        ParamRange, ParamUnit, Params,
    };
    pub use truce_params_derive::{ParamEnum, Params, State};

    // PluginLogic types from hotload prelude (which re-exports from core/gui)
    pub use truce_loader::prelude::{
        Color, PluginLogic, ProcessResult, RenderBackend, Smoother, SmoothingStyle, Theme,
        Transport, WidgetRegion,
    };
}
