//! Reload-transition fixture: `CounterLogic` over `CounterState`.
//!
//! Identical logic to `reload-fixture-keep-a`, exported from a separate
//! crate so the two produce distinct dylib files sharing one state
//! fingerprint - the code-only reload the shell must preserve state
//! across.

use reload_fixture_common::{CounterLogic, FxParams};
use truce::prelude::*;

truce_loader::export_plugin!(CounterLogic, FxParams);
