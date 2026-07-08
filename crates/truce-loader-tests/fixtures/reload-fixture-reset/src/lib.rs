//! Reload-transition fixture: `ResetLogic` over `ResetState`.
//!
//! `ResetState` adds a field to `CounterState`, so its fingerprint
//! differs from keep-a / keep-b. A reload from one of those to this
//! dylib is the layout-changed case: the shell must drop the old state
//! through its origin dylib and re-initialize from this one.

use reload_fixture_common::{FxParams, ResetLogic};
use truce::prelude::*;

truce_loader::export_plugin!(ResetLogic, FxParams);
