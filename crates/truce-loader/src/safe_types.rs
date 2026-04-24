//! Safe types that cross the dylib boundary.
//!
//! Re-exports types from truce-core and truce-gui so there's ONE
//! definition of each type. No duplication.

pub use truce_core::buffer::AudioBuffer;
pub use truce_core::events::{Event, EventBody, EventList, TransportInfo as Transport};
pub use truce_core::process::{ProcessContext, ProcessStatus, ProcessStatus as ProcessResult};
pub use truce_gui::interaction::WidgetRegion;
pub use truce_gui::render::RenderBackend;
pub use truce_gui::theme::{Color, Theme};
pub use truce_gui::widgets::WidgetType as WidgetKind;
