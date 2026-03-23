use std::sync::Arc;

use crate::plugin::Plugin;
use truce_params::Params;

/// Unified export trait for all plugin formats.
///
/// Implement this once on your plugin type. All format wrappers
/// (CLAP, VST3, AU, standalone) use this to construct your plugin
/// and access its parameters.
///
/// ```ignore
/// impl PluginExport for MyPlugin {
///     type Params = MyParams;
///     fn create() -> Self { Self::new() }
///     fn params(&self) -> &MyParams { &self.params }
///     fn params_mut(&mut self) -> &mut MyParams { &mut self.params }
/// }
/// ```
pub trait PluginExport: Plugin + Sized {
    type Params: Params;

    /// Construct a new instance of the plugin.
    fn create() -> Self;

    /// Immutable access to the parameter struct.
    fn params(&self) -> &Self::Params;

    /// Mutable access to the parameter struct.
    fn params_mut(&mut self) -> &mut Self::Params;

    /// Get a shared `Arc` reference to the parameter struct.
    ///
    /// Used by format wrappers to pass params to GUI closures without
    /// raw pointers. The Arc is cloned (cheap ref-count bump), not the
    /// params themselves.
    fn params_arc(&self) -> Arc<Self::Params>;
}
