//! Parameter state bridge between truce's `EditorContext` and Slint UI.
//!
//! Re-exports the cross-toolkit `ParamState` from `truce_gui`. Slint's
//! callback-driven UI relies on the type being `Clone`, which the
//! shared definition provides.

pub use truce_gui::param_state::ParamState;
