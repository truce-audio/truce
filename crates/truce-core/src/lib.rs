pub mod buffer;
pub mod bus;
pub mod chunked_process;
pub mod config;
pub mod custom_state;
pub mod denormal;
pub mod dsp_state;
pub mod editor;
pub mod events;
pub mod export;
pub mod info;
pub mod meters;
pub mod midi;
pub mod plugin;
pub mod presets;
pub mod process;
pub mod rt;
pub mod screenshot;
pub mod snapshot;
pub mod state;
pub mod tasks;
pub mod transport;
pub mod ump;
pub mod util;
pub mod wrapper;

pub use buffer::{AudioBuffer, RawBufferScratch};
pub use bus::{BusConfig, BusKind, BusLayout, ChannelConfig};
pub use config::{AudioConfig, ProcessMode};
pub use dsp_state::{NO_PRESERVE, layout_fingerprint};
pub use editor::{Editor, EditorBuilder, IntoEditor, PluginContext};
pub use events::{Event, EventBody, EventList, PushError, SYSEX_POOL_PREALLOC, TransportInfo};
pub use export::PluginExport;
pub use info::{AutomationConfig, MidiDialect, PluginCategory, PluginInfo};
pub use meters::MeterStore;
pub use plugin::PluginRuntime;
pub use process::{ProcessContext, ProcessStatus};
pub use rt::{RtSection, allow_alloc};
pub use snapshot::SnapshotSlot;

#[cfg(feature = "rt-paranoid")]
pub use rt::RtCheckAlloc;
pub use transport::TransportSlot;

// `Float` / `Sample` live in `truce-params` (truce-core depends on
// truce-params, not the other way around). Re-exported here so
// `truce_core::Float` / `truce_core::sample::Sample` are valid paths
// for callers that don't want to depend on truce-params directly.
pub use truce_params::sample;
pub use truce_params::sample::{Float, Sample};
pub use util::{db_to_linear, linear_to_db, meter_display, midi_note_to_freq};

// `cast`, `shell_sidecar`, and `slugify` are hosted in `truce-utils`
// (a dependency-free crate) so build-time consumers like `cargo-truce`
// can use them without inheriting `truce-core`'s `truce-params` + `png`
// publish chain. Re-exported here under `truce_core::cast::*` etc.
// for callers that already depend on truce-core.
pub use truce_utils::{cast, shell_sidecar, slugify};
