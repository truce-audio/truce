//! ARA (Audio Random Access) support for truce.
//!
//! ARA is a *companion* extension to CLAP / VST3 / AU / AAX (not a
//! standalone format) that gives a plug-in random access to the whole
//! audio timeline for offline analysis and region rendering - what
//! Melodyne / `SpectraLayers` / RX need. A truce plug-in opts in by
//! implementing [`AraPlugin`] alongside its normal `PluginLogic`; the
//! `ara`-feature format wrappers then expose the ARA factory and
//! plug-in extension so an ARA host binds the instance to a document
//! controller.
//!
//! This crate hand-writes the ARA C ABI ([`abi`]) rather than vendoring
//! or `bindgen`-ing the SDK, so enabling ARA needs no external SDK on
//! the author's machine. The ABI is transcribed from the ARA API
//! (Apache-2.0, Celemony); see [`abi`] for the layout contract.
//!
//! Implemented: discovery + document controller (phase 1), the model
//! graph (phase 2), audio access + off-thread analysis (phase 3),
//! playback-region rendering (phase 4), and archive persistence
//! (phase 5).

pub mod abi;
mod archive;
mod arena;
mod audio_source;
mod controller;
mod factory;
mod plugin;
mod renderer;

pub use audio_source::{AnalysisProgress, AraAudioSource, AraReadError};
pub use factory::{ara_factory, null_plugin_extension};
pub use plugin::{AraExport, AraPlugin, AraRenderContext, PlaybackRegion};
pub use renderer::{
    AraBindingCell, PlaybackBinding, PlaybackRenderer, binding_instance, binding_renderer,
    create_playback_binding, destroy_playback_binding,
};
