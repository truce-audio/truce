//! Reload-transition fixture: `CounterLogic` over `CounterState`.
//!
//! Paired with `reload-fixture-keep-b`, which exports the identical
//! logic from a separate crate - a different dylib file (so a reload
//! actually rebuilds) with the same `save_state` / `load_state` format
//! (so the reload carries the live state over).

use reload_fixture_common::{CounterLogic, FxParams};
use truce::prelude::*;

truce_loader::export_plugin!(CounterLogic, FxParams);
