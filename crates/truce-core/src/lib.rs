pub mod buffer;
pub mod bus;
pub mod custom_state;
pub mod denormal;
pub mod editor;
pub mod events;
pub mod export;
pub mod info;
pub mod midi;
pub mod plugin;
pub mod process;
pub mod screenshot;
pub mod state;
pub mod transport;
pub mod ump;
pub mod util;
pub mod wrapper;

pub use buffer::{AudioBuffer, RawBufferScratch};
pub use bus::{BusConfig, BusKind, BusLayout, ChannelConfig};
pub use editor::{Editor, PluginContext};
pub use events::{Event, EventBody, EventList, PushError, SYSEX_POOL_PREALLOC, TransportInfo};
pub use export::PluginExport;
pub use info::{PluginCategory, PluginInfo};
pub use plugin::PluginRuntime;
pub use process::{ProcessContext, ProcessStatus};
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
