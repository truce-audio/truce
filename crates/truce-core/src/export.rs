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

    /// Shared meter storage handle, mirroring [`Self::params_arc`]:
    /// the audio thread publishes meter values into this store from
    /// inside `process()` (via the shells' meter callback), and GUI
    /// `get_meter` closures read it - never the plugin instance,
    /// whose `&mut` the audio thread holds for the whole block.
    ///
    /// The `truce::plugin!` shells own the store and return their
    /// handle here; a hand-written `PluginExport` impl keeps one
    /// alongside its params `Arc`.
    fn meter_store(&self) -> Arc<crate::meters::MeterStore>;

    /// Shared snapshot slot for lock-free state save. The shell (audio
    /// thread) publishes the plugin's custom state into it after each
    /// block when the plugin overrides `snapshot_into`; the wrapper's
    /// `save_state` reads it without taking the plugin lock. A plugin
    /// that doesn't opt in never publishes, so the slot stays empty and
    /// `save_state` falls back to the locked path.
    ///
    /// The `truce::plugin!` shells own the slot and return their handle
    /// here; a hand-written impl keeps one alongside its params `Arc`.
    fn snapshot_slot(&self) -> Arc<crate::snapshot::SnapshotSlot>;

    /// A lock-free editor builder: hand it the param store and it
    /// returns the editor (or `None` for a headless plugin).
    ///
    /// Called once at instance creation - format wrappers cache the
    /// returned closure *outside* the plugin lock (alongside
    /// `params_arc`), then invoke it when the host opens the GUI, so
    /// editor construction never takes the plugin lock and never waits
    /// on an in-flight audio block. The closure binds only the
    /// lock-free param store, so a `--shell` build's closure rebuilds
    /// from the *reloaded* dylib (GUI hot-reload survives, picked up on
    /// the next editor close+open). The `truce::plugin!` shells provide
    /// this; a hand-written impl returns a closure that builds its
    /// editor directly. Default: a closure that returns `None`.
    fn editor_builder(&self) -> crate::editor::EditorBuilder<Self::Params> {
        Box::new(|_params| None)
    }

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
        let plugin = Self::create();
        plugin.editor_builder()(plugin.params_arc()).is_some()
    }
}
