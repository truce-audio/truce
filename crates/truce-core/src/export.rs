use std::sync::Arc;

use crate::plugin::PluginRuntime;
use truce_params::{ParamInfo, Params};

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
///     fn params_arc(&self) -> Arc<MyParams> { self.params.clone() }
/// }
/// ```
///
/// All parameter mutation goes through the atomic-backed accessors on
/// `&Params` - no `&mut Params` accessor is required, which keeps the
/// trait usable while the editor holds an `Arc<Params>` reader.
pub trait PluginExport: PluginRuntime + Sized {
    type Params: Params;

    /// Construct a new instance of the plugin.
    fn create() -> Self;

    /// Immutable access to the parameter struct.
    fn params(&self) -> &Self::Params;

    /// Get a shared `Arc` reference to the parameter struct.
    ///
    /// Used by format wrappers to pass params to GUI closures without
    /// raw pointers. The Arc is cloned (cheap ref-count bump), not the
    /// params themselves.
    fn params_arc(&self) -> Arc<Self::Params>;

    /// Static parameter metadata for registration-time access.
    ///
    /// Format wrappers' `register_*` paths (`truce-vst2`, `truce-vst3`,
    /// `truce-au`, `truce-aax`) call this instead of the historical
    /// `Self::create().params().param_infos()` walk, which constructed
    /// a full plugin instance - including any allocation the
    /// constructor did (DSP buffers, FFT plans, image atlases) - just
    /// to read static metadata. On platforms where registration runs
    /// from C++ static initializers (notably AAX `Describe`) those
    /// allocations sit in a fragile init-order regime; avoiding them
    /// closes a class of platform-dependent registration bugs.
    ///
    /// Default impl prefers
    /// [`Params::param_infos_static`]
    /// when it returns a non-empty vec (the `#[derive(Params)]` path
    /// emits an override built from compile-time metadata) and falls
    /// back to the runtime construction otherwise - so plugins with
    /// hand-written `Params` impls that don't override the static
    /// path keep working unchanged.
    #[must_use]
    fn param_infos_static() -> Vec<ParamInfo> {
        let from_params = <Self::Params as Params>::param_infos_static();
        if from_params.is_empty() {
            Self::create().params().param_infos()
        } else {
            from_params
        }
    }

    /// Static "does this plugin have an editor" predicate. AAX's
    /// `Describe` path needs to know this at registration time
    /// (`has_editor` field on the static descriptor). Paired with
    /// [`Self::param_infos_static`], this is the second reason every
    /// format's registration walk constructed a plugin.
    ///
    /// Default impl falls back to that runtime path so unannotated
    /// plugins keep working. Plugins that want to avoid the
    /// static-init plugin construction (notably for AAX hosts that
    /// run `Describe` very early) override with a `const`-style
    /// answer:
    ///
    /// ```ignore
    /// impl PluginExport for MyPlugin {
    ///     // ...
    ///     fn has_editor_static() -> bool { true }
    /// }
    /// ```
    ///
    /// VST2 / VST3 / AU never call this - they don't need the answer
    /// at registration time.
    #[must_use]
    fn has_editor_static() -> bool {
        Self::create().editor().is_some()
    }
}
