pub mod buffer;
pub mod bus;
pub mod custom_state;
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
pub mod util;
pub mod wrapper;

pub use buffer::{AudioBuffer, RawBufferScratch};
pub use bus::{BusConfig, BusKind, BusLayout, ChannelConfig};
pub use editor::{Editor, PluginContext};
pub use events::{Event, EventBody, EventList, TransportInfo};
pub use export::PluginExport;
pub use info::{PluginCategory, PluginInfo};
pub use plugin::Plugin;
pub use process::{ProcessContext, ProcessStatus};
pub use transport::TransportSlot;
pub use util::{db_to_linear, linear_to_db, meter_display, midi_note_to_freq};

// `cast`, `shell_sidecar`, and `slugify` are hosted in `truce-utils`
// (a dependency-free crate) so build-time consumers like `cargo-truce`
// can use them without inheriting `truce-core`'s `truce-params` + `png`
// publish chain. Re-exported here so existing callers
// (`truce_core::cast::*`, `truce_core::slugify`,
// `truce_core::shell_sidecar`) keep compiling unchanged.
pub use truce_utils::{cast, shell_sidecar, slugify};
