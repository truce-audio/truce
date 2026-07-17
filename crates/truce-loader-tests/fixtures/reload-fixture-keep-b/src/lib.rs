//! Reload-transition fixture: `CounterLogic` over `CounterState`.
//!
//! Identical logic to `reload-fixture-keep-a`, exported from a separate
//! crate so the two produce distinct dylib files sharing one
//! `save_state` / `load_state` format - the reload whose live state the
//! shell carries over.

use reload_fixture_common::{CounterLogic, FxParams};
use truce::prelude::*;

truce_loader::export_plugin!(CounterLogic, FxParams);
