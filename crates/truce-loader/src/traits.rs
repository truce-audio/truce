//! Combined trait bound for the dylib boundary.
//!
//! [`LoaderPlugin`] joins [`truce_core::PluginLogic`] (the DSP
//! surface, in `truce-core`) and [`truce_gui::PluginEditor`] (the
//! GUI surface, in `truce-gui`) into a single trait object the
//! shell holds and the dylib exports. Trait objects can only name
//! one non-auto trait, so a supertrait is the only way to package
//! both vtables behind one `Box<dyn _>`.
//!
//! Plugin authors don't implement `LoaderPlugin` directly — the
//! blanket impl below derives it from any type that implements
//! both halves. They write `impl PluginLogic for X` (DSP) and
//! `impl PluginEditor for X` (GUI), and the `truce::plugin!`
//! macro plus the `Box<dyn LoaderPlugin>` ABI take care of the
//! rest.

use truce_core::PluginLogic;
use truce_gui::PluginEditor;

/// The dylib-boundary trait object: `Box<dyn LoaderPlugin>`. Any
/// type implementing both [`PluginLogic`] and [`PluginEditor`]
/// satisfies it via the blanket impl below.
pub trait LoaderPlugin: PluginLogic + PluginEditor {}

impl<T: PluginLogic + PluginEditor> LoaderPlugin for T {}
