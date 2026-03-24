//! Hot-reloadable plugin logic for truce.
//!
//! Split your plugin into a static shell (loaded by the DAW) and a
//! hot-reloadable logic dylib (reloads on recompile). The developer
//! implements [`PluginLogic`] — a safe Rust trait — and exports it
//! via `#[no_mangle]` functions. The shell loads the dylib, verifies
//! ABI compatibility, and delegates audio processing + GUI rendering
//! to the trait object.
//!
//! # For the logic dylib
//!
//! ```ignore
//! use truce_loader::prelude::*;
//!
//! struct MyPlugin { /* ... */ }
//! impl PluginLogic for MyPlugin { /* ... */ }
//!
//! #[no_mangle]
//! pub fn truce_create() -> Box<dyn PluginLogic> { Box::new(MyPlugin::new()) }
//!
//! #[no_mangle]
//! pub fn truce_abi_canary() -> AbiCanary { AbiCanary::current() }
//!
//! #[no_mangle]
//! pub fn truce_vtable_probe() -> Box<dyn PluginLogic> { Box::new(ProbePlugin) }
//! ```

#[doc(hidden)]
pub mod __macro_deps {
    pub use truce_core;
}

mod safe_types;
mod traits;
mod canary;

#[cfg(feature = "shell")]
mod loader;
#[cfg(feature = "shell")]
pub mod shell;
pub mod static_shell;

pub use safe_types::*;
pub use traits::*;
pub use canary::{AbiCanary, ProbePlugin, verify_probe};

#[cfg(feature = "shell")]
pub use loader::NativeLoader;

/// Export the three `#[no_mangle]` functions required by the shell.
///
/// ```ignore
/// struct MyPlugin { /* ... */ }
/// impl PluginLogic for MyPlugin { /* ... */ }
///
/// export_plugin!(MyPlugin);
/// ```
#[macro_export]
macro_rules! export_plugin {
    ($ty:ty) => {
        #[no_mangle]
        pub fn truce_create() -> Box<dyn $crate::PluginLogic> {
            Box::new(<$ty as $crate::PluginLogic>::new())
        }

        #[no_mangle]
        pub fn truce_abi_canary() -> $crate::AbiCanary {
            $crate::AbiCanary::current()
        }

        #[no_mangle]
        pub fn truce_vtable_probe() -> Box<dyn $crate::PluginLogic> {
            Box::new($crate::ProbePlugin)
        }
    };
}

/// Convenience prelude for logic dylib authors.
pub mod prelude {
    pub use crate::safe_types::*;
    pub use crate::traits::*;
    pub use crate::canary::{AbiCanary, ProbePlugin};

    // Re-export param types so the developer can own params in their struct.
    pub use truce_params::{Params, FloatParam, BoolParam, IntParam, EnumParam, ParamEnum};
    pub use truce_params::{Smoother, SmoothingStyle};

    // Re-export utility functions.
    pub use truce_core::util::{db_to_linear, linear_to_db, midi_note_to_freq};
}
