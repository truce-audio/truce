//! Reload-transition fixture: `ResetLogic` over `ResetState`.
//!
//! `ResetLogic` defines no `save_state` / `load_state`, so a reload from
//! keep-a / keep-b to this dylib can't restore the carried blob: the
//! shell drops the old state through its origin dylib and re-initializes
//! from this one - the sound fallback when carry-over isn't serialized.

use reload_fixture_common::{FxParams, ResetLogic};
use truce::prelude::*;

truce_loader::export_plugin!(ResetLogic, FxParams);
