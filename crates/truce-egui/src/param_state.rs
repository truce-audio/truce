//! Parameter state bridge between truce's `EditorContext` and egui widgets.
//!
//! Wraps the `begin_edit` / `set_param` / `end_edit` host protocol into
//! ergonomic accessors that egui widgets can call during a frame. The
//! type itself lives in `truce_gui::param_state`; both truce-egui and
//! truce-slint re-export it so plugin authors see the same surface
//! regardless of toolkit.

pub use truce_gui::param_state::ParamState;
