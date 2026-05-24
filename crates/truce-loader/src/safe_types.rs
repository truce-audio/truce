//! Safe types that cross the dylib boundary.
//!
//! Re-exports types from `truce-core` and `truce-gui-types` so there's
//! ONE definition of each type. No duplication. Sourced from
//! `truce-gui-types` (the lightweight types crate) rather than the
//! heavier `truce-gui` renderer so the canary stays buildable when
//! `builtin-gui` is off.

pub use truce_core::buffer::AudioBuffer;
pub use truce_core::events::{Event, EventBody, EventList, TransportInfo};
pub use truce_core::process::{ProcessContext, ProcessStatus};
pub use truce_gui_types::interaction::WidgetRegion;
pub use truce_gui_types::theme::{Color, Theme};
