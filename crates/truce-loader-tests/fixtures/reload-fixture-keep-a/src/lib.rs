//! Reload-transition fixture: `CounterLogic` over `CounterState`.
//!
//! Paired with `reload-fixture-keep-b`, which exports the identical
//! logic from a separate crate - a different dylib file (so a reload
//! actually rebuilds) with the same state fingerprint (so the reload
//! must preserve the live state).

use reload_fixture_common::{CounterLogic, FxParams};
use truce::prelude::*;

truce_loader::export_plugin!(CounterLogic, FxParams);
