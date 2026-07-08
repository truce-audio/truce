//! Hot-reload mechanics for truce: dylib loading, ABI canary, and the
//! shells (`HotShell<P, S>`, `StaticShell<P, L, S>`) that bridge the
//! user-facing `truce_plugin::PluginLogic` / `truce_plugin::PluginLogic64`
//! leaf traits onto [`truce_core::PluginRuntime`] for format wrappers.
//!
//! Plugin authors don't reach into this crate directly. They write
//! `impl PluginLogic for MyPlugin` (the leaf trait is sample-pinned
//! via the prelude re-export) and the `truce::plugin!` macro picks
//! the static or hot shell based on the `shell` Cargo feature.
//!
//! # ABI boundary
//!
//! `PluginLogic` is a stateless descriptor with a separate `type DspState`,
//! so the DSP state can live in the *shell* rather than the reloadable
//! dylib. The dylib exports a flat set of Rust-ABI functions
//! (`export_plugin!`) over an opaque `*mut ()` state pointer (an erased
//! `Box<State>`) plus the shell's `Arc<Params>` pointer. `HotShell` owns
//! the state and, on a reload, keeps it when the new dylib's
//! `truce_state_fingerprint` matches (code-only edit) - so a reverb tail
//! survives the swap - and re-inits it otherwise. `StaticShell` holds a
//! typed `L::DspState` directly.
//!
//! ```ignore
//! use truce_loader::{AbiCanary, PluginLogic, PluginLogicCore};
//!
//! struct MyPlugin;                 // stateless descriptor
//! impl PluginLogic for MyPlugin { type DspState = MyState; /* ... */ }
//!
//! // Emitted by `truce::plugin!` (plugin authors don't write these).
//! // `Sample` resolves through the prelude alias (`f32` for `prelude` /
//! // `prelude32` / `prelude64m`, `f64` for `prelude64`).
//! #[unsafe(no_mangle)]
//! pub fn truce_init_state(params: *const ()) -> *mut () { /* Box<State> */ }
//! #[unsafe(no_mangle)]
//! pub fn truce_process(state: *mut (), params: *const (), /* ... */) { }
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
}

mod canary;
mod safe_types;

#[cfg(feature = "shell")]
mod loader;
#[cfg(feature = "shell")]
pub mod shell;
pub mod static_shell;

pub use canary::{ABI_EPOCH, AbiCanary};
pub use safe_types::*;
// Source the leaf + core traits directly from `truce-plugin` rather
// than via the optional `truce-gui` re-export, so these names are
// reachable regardless of whether the `builtin-gui` feature is on.
pub use truce_plugin::{PluginLogic, PluginLogic64, PluginLogicCore};

#[cfg(feature = "shell")]
pub use loader::NativeLoader;

/// Export the `#[unsafe(no_mangle)]` symbols the hot-reload shell binds.
///
/// The dylib no longer hands the shell a `Box<dyn PluginLogicCore>`
/// trait object. Instead it exports a flat set of Rust-ABI functions
/// that operate on an **opaque state pointer** (`*mut ()`, an erased
/// `Box<State>`): the shell owns the state, so it can hold it across a
/// hot-reload code swap and run the freshly loaded code on the same
/// bytes (guarded by the `truce_state_fingerprint` export).
///
/// `params_ptr` is a raw `Arc<Params>` pointer from the shell; each call
/// borrows `&Params` from it (no refcount change - the shell keeps the
/// `Arc` alive for the call's duration). `Sample` is the prelude's
/// `type Sample` alias (`f32` for `prelude` / `prelude32` / `prelude64m`,
/// `f64` for `prelude64`); the canary's `sample_precision` byte guards a
/// precision-mismatched load.
#[macro_export]
macro_rules! export_plugin {
    ($logic:ty, $params:ty) => {
        /// Build the initial DSP state; returns an erased `Box<State>`.
        #[unsafe(no_mangle)]
        pub fn truce_init_state(params_ptr: *const ()) -> *mut () {
            let params: &$params = unsafe { &*(params_ptr as *const $params) };
            let state = <$logic as $crate::PluginLogicCore<Sample>>::init(params);
            Box::into_raw(Box::new(state)).cast::<()>()
        }

        /// Drop a state allocated by *this* dylib's `truce_init_state`.
        /// Called by the shell through the origin dylib (kept alive by
        /// the loader's leaked-handle policy) so `State`'s `Drop` runs
        /// with the code that produced it.
        #[unsafe(no_mangle)]
        pub fn truce_drop_state(state: *mut ()) {
            drop(unsafe {
                Box::from_raw(state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>())
            });
        }

        /// Structural fingerprint of `State` - the shell keeps the old
        /// state across a reload only when this matches.
        #[unsafe(no_mangle)]
        pub fn truce_state_fingerprint() -> u64 {
            <$logic as $crate::PluginLogicCore<Sample>>::STATE_FINGERPRINT
        }

        #[unsafe(no_mangle)]
        pub fn truce_reset(
            state: *mut (),
            params_ptr: *const (),
            config: &$crate::__macro_deps::truce_core::config::AudioConfig,
        ) {
            let state = unsafe {
                &mut *state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>()
            };
            let params: &$params = unsafe { &*(params_ptr as *const $params) };
            <$logic as $crate::PluginLogicCore<Sample>>::reset(state, params, config);
        }

        #[unsafe(no_mangle)]
        pub fn truce_process(
            state: *mut (),
            params_ptr: *const (),
            buffer: &mut $crate::__macro_deps::truce_core::buffer::AudioBuffer<Sample>,
            events: &$crate::__macro_deps::truce_core::events::EventList,
            ctx: &mut $crate::__macro_deps::truce_core::process::ProcessContext,
        ) -> $crate::__macro_deps::truce_core::process::ProcessStatus {
            let state = unsafe {
                &mut *state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>()
            };
            let params: &$params = unsafe { &*(params_ptr as *const $params) };
            <$logic as $crate::PluginLogicCore<Sample>>::process(state, params, buffer, events, ctx)
        }

        #[unsafe(no_mangle)]
        pub fn truce_latency(state: *const ()) -> u32 {
            let state =
                unsafe { &*state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>() };
            <$logic as $crate::PluginLogicCore<Sample>>::latency(state)
        }

        #[unsafe(no_mangle)]
        pub fn truce_tail(state: *const ()) -> u32 {
            let state =
                unsafe { &*state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>() };
            <$logic as $crate::PluginLogicCore<Sample>>::tail(state)
        }

        #[unsafe(no_mangle)]
        pub fn truce_save_state(state: *const ()) -> Vec<u8> {
            let state =
                unsafe { &*state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>() };
            <$logic as $crate::PluginLogicCore<Sample>>::save_state(state)
        }

        #[unsafe(no_mangle)]
        pub fn truce_snapshot_into(state: *const (), buf: &mut Vec<u8>) -> bool {
            let state =
                unsafe { &*state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>() };
            <$logic as $crate::PluginLogicCore<Sample>>::snapshot_into(state, buf)
        }

        #[unsafe(no_mangle)]
        pub fn truce_load_state(
            state: *mut (),
            data: &[u8],
        ) -> Result<(), $crate::__macro_deps::truce_core::state::StateLoadError> {
            let state = unsafe {
                &mut *state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>()
            };
            <$logic as $crate::PluginLogicCore<Sample>>::load_state(state, data)
        }

        #[unsafe(no_mangle)]
        pub fn truce_state_changed(state: *mut (), params_ptr: *const ()) {
            let state = unsafe {
                &mut *state.cast::<<$logic as $crate::PluginLogicCore<Sample>>::DspState>()
            };
            let params: &$params = unsafe { &*(params_ptr as *const $params) };
            <$logic as $crate::PluginLogicCore<Sample>>::state_changed(state, params);
        }

        // Editor construction lives in its own symbol: it is
        // receiverless (over the shared `Arc<Params>`), so the shell
        // rebuilds the editor from this dylib's `$logic` without
        // touching the DSP state. A reload swaps in the new editor
        // code - the host picks it up on the next editor close+open.
        #[unsafe(no_mangle)]
        pub fn truce_build_editor(
            params_ptr: *const (),
        ) -> Box<dyn $crate::__macro_deps::truce_core::editor::Editor> {
            let params: Arc<$params> = unsafe {
                Arc::increment_strong_count(params_ptr as *const $params);
                Arc::from_raw(params_ptr as *const $params)
            };
            <$logic as $crate::__macro_deps::truce_plugin::PluginEditor<Sample>>::editor(params)
        }

        // `_v2` because `AbiCanary` crosses this boundary *by value*
        // (sret): if the two sides disagreed about its size, the call
        // itself would corrupt the caller's stack before any field
        // compare. A canary-layout change therefore renames the symbol,
        // so a mismatched pair fails at `dlsym` - cleanly.
        #[unsafe(no_mangle)]
        pub fn truce_abi_canary_v2() -> $crate::AbiCanary {
            $crate::AbiCanary::current::<Sample>()
        }
    };
}
