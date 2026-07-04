//! Hot-reload mechanics for truce: dylib loading, ABI canary,
//! vtable probe, and the shells (`HotShell<P, S>`, `StaticShell<P, L, S>`)
//! that bridge the user-facing `truce_plugin::PluginLogic` /
//! `truce_plugin::PluginLogic64` leaf traits onto
//! [`truce_core::PluginRuntime`] for format wrappers.
//!
//! Plugin authors don't reach into this crate directly. They write
//! `impl PluginLogic for MyPlugin` (the leaf trait is sample-pinned
//! via the prelude re-export) and the `truce::plugin!` macro picks
//! the static or hot shell based on the `shell` Cargo feature.
//!
//! # ABI boundary
//!
//! Across the dylib boundary the shell holds a
//! `Box<dyn truce_plugin::PluginLogicCore<S>>` - the generic
//! wrapper-facing trait that both leaf traits forward into via
//! blanket impls in `truce-plugin`. The single trait object
//! carries DSP and GUI methods through one vtable, with `S` baked
//! in by the shell's generic parameter (and recorded in
//! `AbiCanary::sample_precision` so a precision mismatch fails
//! the canary check before vtable-binding).
//!
//! ```ignore
//! use truce_loader::{AbiCanary, PluginLogic, PluginLogicCore};
//!
//! struct MyPlugin { /* ... */ }
//! impl PluginLogic for MyPlugin { /* DSP + GUI */ }
//!
//! // Emitted by `truce::plugin!`; plugin authors don't write these
//! // by hand. `Sample` resolves through the prelude alias
//! // (`f32` for `prelude` / `prelude32` / `prelude64m`,
//! // `f64` for `prelude64`). The macro also emits a
//! // `truce_vtable_probe` symbol that constructs an internal
//! // `ProbePlugin`; that type lives in `__macro_deps` and isn't
//! // intended for direct use.
//! #[unsafe(no_mangle)]
//! pub fn truce_create(p: *const ()) -> Box<dyn PluginLogicCore<Sample>> {
//!     Box::new(MyPlugin::new(/* params from p */))
//! }
//!
//! #[unsafe(no_mangle)]
//! pub fn truce_abi_canary_v2() -> AbiCanary { AbiCanary::current::<Sample>() }
//! ```

#[doc(hidden)]
pub mod __macro_deps {
    pub use truce_core;
    // `truce_plugin` carries the `PluginLogicCore` blanket the
    // `export_plugin!` / `export_static!` macros need to name
    // (`<L as PluginLogicCore<Sample>>::supports_in_place()` etc.).
    // Re-exported here so the macro can resolve it via
    // `$crate::__macro_deps::truce_plugin` regardless of whether the
    // caller has `truce-plugin` as a direct dep.
    pub use truce_plugin;
    // `ProbePlugin` is a vtable-binding shim emitted into every
    // `export_plugin!` expansion as the `truce_vtable_probe` symbol.
    // It is not a public type; plugin authors never name it. Kept
    // reachable here under `$crate::__macro_deps::` so the macro can
    // resolve it without leaking the type at the crate root.
    pub use crate::canary::ProbePlugin;
}

mod canary;
mod safe_types;

#[cfg(feature = "shell")]
mod loader;
#[cfg(feature = "shell")]
pub mod shell;
pub mod static_shell;

pub use canary::AbiCanary;
// `ProbePlugin`, `verify_probe`, and `ProbeError` are loader-internal.
// `ProbePlugin` lives under `__macro_deps` so the `export_plugin!`
// macro's `truce_vtable_probe` body can name it without leaking the
// type at the crate root. `verify_probe` / `ProbeError` are only used
// by `NativeLoader::build_candidate` (gated on `feature = "shell"`)
// and are reached via `crate::canary` directly inside `loader.rs`.
// Format wrappers and plugin authors reach the probe by dlopening
// the `truce_vtable_probe` symbol the macro emits, not by `use` import.
pub use safe_types::*;
// Source the leaf + core traits directly from `truce-plugin` rather
// than via the optional `truce-gui` re-export, so these names are
// reachable regardless of whether the `builtin-gui` feature is on.
pub use truce_plugin::{PluginLogic, PluginLogic64, PluginLogicCore};

#[cfg(feature = "shell")]
pub use loader::NativeLoader;

/// Export the `#[unsafe(no_mangle)]` functions required by the shell.
///
/// `params_ptr` is a raw `Arc<Params>` pointer from the shell.
/// The plugin receives shared params - one copy, no sync.
#[macro_export]
macro_rules! export_plugin {
    ($logic:ty, $params:ty) => {
        // `Sample` here is the prelude's `type Sample = ...` alias:
        // `f32` for `prelude` / `prelude32` / `prelude64m`, `f64` for
        // `prelude64`. `HotShell<P, S>` is generic over `S: Sample`
        // so both precisions hot-reload through the same dylib export
        // shape; the canary's `sample_precision` byte at load time
        // guards against a shell built for one precision dlopening a
        // dylib built for the other.
        #[unsafe(no_mangle)]
        pub fn truce_create(params_ptr: *const ()) -> Box<dyn $crate::PluginLogicCore<Sample>> {
            let params: Arc<$params> = unsafe {
                Arc::increment_strong_count(params_ptr as *const $params);
                Arc::from_raw(params_ptr as *const $params)
            };
            // The plugin impls one of the leaf traits
            // (`PluginLogic` for f32 or `PluginLogic64` for f64); the
            // blanket impl inside `truce-plugin` gives it
            // `PluginLogicCore<Sample>` automatically, so the cast
            // here just picks the right vtable.
            Box::new(<$logic>::new(params))
        }

        // `_v2` because `AbiCanary` crosses this boundary *by value*
        // (sret): if the two sides disagreed about its size, the call
        // itself would corrupt the caller's stack before any field
        // compare. A canary-layout change therefore renames the
        // symbol, so a mismatched pair fails at `dlsym` - cleanly -
        // instead. (v2: added `abi_epoch`.)
        #[unsafe(no_mangle)]
        pub fn truce_abi_canary_v2() -> $crate::AbiCanary {
            // `Sample` from the prelude - the dylib stamps its
            // chosen precision into the canary so the shell can
            // reject a mismatched load before vtable-binding.
            $crate::AbiCanary::current::<Sample>()
        }

        #[unsafe(no_mangle)]
        pub fn truce_vtable_probe() -> Box<dyn $crate::PluginLogicCore<Sample>> {
            Box::new($crate::__macro_deps::ProbePlugin::default())
        }
    };
}
