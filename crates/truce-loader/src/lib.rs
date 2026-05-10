//! Hot-reload mechanics for truce: dylib loading, ABI canary,
//! vtable probe, and the shells (`HotShell`, `StaticShell`) that
//! bridge the user-facing [`truce_core::PluginLogic`] (DSP) and
//! [`truce_gui::PluginEditor`] (GUI) traits onto
//! [`truce_core::Plugin`] for format wrappers.
//!
//! Plugin authors don't reach into this crate directly. They write
//! `impl PluginLogic for MyPlugin` and `impl PluginEditor for MyPlugin`,
//! and the `truce::plugin!` macro picks the static or hot shell
//! based on the `shell` Cargo feature.
//!
//! # ABI boundary
//!
//! Across the dylib boundary the shell holds a [`Box<dyn LoaderPlugin>`]
//! — a supertrait that combines `PluginLogic` and `PluginEditor`
//! into one trait object (Rust trait objects can name only one
//! non-auto trait). Any type implementing both halves satisfies it
//! via the blanket `impl<T: PluginLogic + PluginEditor> LoaderPlugin for T {}`.
//!
//! ```ignore
//! use truce_loader::prelude::*;
//!
//! struct MyPlugin { /* ... */ }
//! impl PluginLogic for MyPlugin { /* DSP */ }
//! impl PluginEditor for MyPlugin { /* GUI */ }
//!
//! #[unsafe(no_mangle)]
//! pub fn truce_create() -> Box<dyn LoaderPlugin> { Box::new(MyPlugin::new()) }
//!
//! #[unsafe(no_mangle)]
//! pub fn truce_abi_canary() -> AbiCanary { AbiCanary::current() }
//!
//! #[unsafe(no_mangle)]
//! pub fn truce_vtable_probe() -> Box<dyn LoaderPlugin> { Box::new(ProbePlugin::default()) }
//! ```

#[doc(hidden)]
pub mod __macro_deps {
    pub use truce_core;
}

mod canary;
mod safe_types;

#[cfg(feature = "shell")]
mod loader;
#[cfg(feature = "shell")]
pub mod shell;
pub mod static_shell;

pub use canary::{AbiCanary, ProbePlugin, verify_probe};
pub use safe_types::*;

use truce_core::PluginLogic;
use truce_gui::PluginEditor;

/// The dylib-boundary trait object: `Box<dyn LoaderPlugin>`. Trait
/// objects can name only one non-auto trait, so a supertrait is the
/// only way to package both `PluginLogic` (DSP) and `PluginEditor`
/// (GUI) vtables behind one `Box<dyn _>`.
///
/// Plugin authors don't implement this directly — the blanket impl
/// derives it from any type that implements both halves.
pub trait LoaderPlugin: PluginLogic + PluginEditor {}

impl<T: PluginLogic + PluginEditor> LoaderPlugin for T {}

#[cfg(feature = "shell")]
pub use loader::NativeLoader;

/// Export the `#[unsafe(no_mangle)]` functions required by the shell.
///
/// `params_ptr` is a raw `Arc<Params>` pointer from the shell.
/// The plugin receives shared params — one copy, no sync.
#[macro_export]
macro_rules! export_plugin {
    ($logic:ty, $params:ty) => {
        #[unsafe(no_mangle)]
        pub fn truce_create(params_ptr: *const ()) -> Box<dyn $crate::LoaderPlugin> {
            let params: Arc<$params> = unsafe {
                Arc::increment_strong_count(params_ptr as *const $params);
                Arc::from_raw(params_ptr as *const $params)
            };
            Box::new(<$logic>::new(params))
        }

        #[unsafe(no_mangle)]
        pub fn truce_abi_canary() -> $crate::AbiCanary {
            $crate::AbiCanary::current()
        }

        #[unsafe(no_mangle)]
        pub fn truce_vtable_probe() -> Box<dyn $crate::LoaderPlugin> {
            Box::new($crate::ProbePlugin::default())
        }
    };
}

/// Convenience prelude for logic dylib authors.
pub mod prelude {
    pub use crate::LoaderPlugin;
    pub use crate::canary::{AbiCanary, ProbePlugin};
    pub use crate::safe_types::*;

    pub use truce_core::PluginLogic;
    pub use truce_gui::PluginEditor;

    // Re-export param types so the developer can own params in their struct.
    pub use truce_params::{BoolParam, EnumParam, FloatParam, IntParam, ParamEnum, Params};
    pub use truce_params::{Smoother, SmoothingStyle};

    // Re-export utility functions.
    pub use truce_core::util::{db_to_linear, linear_to_db, midi_note_to_freq};
}
