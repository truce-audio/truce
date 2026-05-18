//! Slint GUI backend for truce audio plugins.
//!
//! Provides `SlintEditor`, which implements `truce_core::Editor` using
//! Slint's software renderer + baseview + wgpu. Developers write their UI
//! in `.slint` markup (compiled at build time) and wire parameters through
//! `PluginContext<P>`.
//!
//! # Usage
//!
//! ```ignore
//! use truce_slint::SlintEditor;
//! use truce_core::editor::PluginContext;
//!
//! SlintEditor::new(params, (400, 300), |state: PluginContext<MyParams>| {
//!     let ui = MyPluginUi::new().unwrap();
//!     truce_slint::bind! { state, ui,
//!         P::Gain   => gain,
//!         P::Pan    => pan,
//!         P::Bypass => bypass: bool,
//!     }
//! })
//! ```

// baseview + wgpu live behind `blit` + `editor` on non-iOS hosts.
// iOS uses `editor_ios.rs` which runs the same Slint
// `MinimalSoftwareWindow` CPU renderer + blits via `CGImage`
// (skipping baseview / wgpu entirely). `platform.rs` carries the
// Slint platform-registration glue - needed on every target;
// inside the file, the baseview / wgpu re-exports are themselves
// cfg-gated.
#[cfg(not(target_os = "ios"))]
pub mod blit;
#[cfg(not(target_os = "ios"))]
pub mod editor;
pub mod platform;
#[cfg(not(target_os = "ios"))]
mod screenshot;

#[cfg(target_os = "ios")]
mod editor_ios;

#[cfg(not(target_os = "ios"))]
pub use editor::{SlintEditor, SyncFn};

#[cfg(target_os = "ios")]
pub use editor_ios::{SlintEditor, SyncFn};

// Re-export `PluginContext` so plugin authors using the `bind!` macro
// don't need a direct truce-core dependency.
pub use truce_core::editor::PluginContext;

// Re-export slint so plugin authors can use it without a direct dependency.
pub use slint;

// Re-export paste (used by the bind! macro).
#[doc(hidden)]
pub use paste::paste;

// Re-export truce_core (used by the bind! macro for cast helpers).
#[doc(hidden)]
pub use truce_core;

/// Bind Slint properties to truce parameters.
///
/// Generates both the `on_<name>_changed` callback wiring (UI → host) and
/// returns a sync closure (host → UI) called each frame.
///
/// # Syntax
///
/// ```ignore
/// truce_slint::bind! { state, ui,
///     PARAM_ID => property_name,              // float (default)
///     PARAM_ID => property_name: bool,        // boolean
/// }
/// ```
///
/// `property_name` must match the Slint property name. The macro calls
/// `ui.on_<name>_changed(...)` and `ui.set_<name>(...)` via identifier
/// concatenation.
///
/// # Example
///
/// ```ignore
/// let ui = MyPluginUi::new().unwrap();
/// truce_slint::bind! { state, ui,
///     P::Gain   => gain,
///     P::Pan    => pan,
///     P::Bypass => bypass: bool,
/// }
/// ```
#[macro_export]
macro_rules! bind {
    ($state:expr, $ui:expr, $( $id:expr => $name:ident $( : $ty:ident $(($arg:expr))? )? ),* $(,)?) => {{
        $(
            $crate::bind!(@wire $state, $ui, $id, $name $( : $ty $(($arg))? )?);
        )*
        let ui = $ui;
        // Return type is inferred from the surrounding `SetupFn` -
        // typically `SyncFn<P>` aka `Box<dyn Fn(&PluginContext<P>)>`.
        Box::new(move |state: &$crate::PluginContext<_>| {
            $(
                $crate::bind!(@sync state, ui, $id, $name $( : $ty $(($arg))? )?);
            )*
        })
    }};

    // -- float (default) --
    (@wire $state:expr, $ui:expr, $id:expr, $name:ident) => {
        {
            let s = $state.clone();
            let id: u32 = $id.into();
            $crate::paste! {
                $ui.[<on_ $name _changed>](move |v| s.automate(id, v as f64));
            }
        }
    };
    (@sync $state:expr, $ui:expr, $id:expr, $name:ident) => {
        $crate::paste! {
            // `state.get_param` resolves through the user's
            // prelude's `PluginContextReadF{32,64}` trait - could
            // be either precision. `.to_f32()` narrows uniformly,
            // matching slint's `f32`-typed property setter.
            $ui.[<set_ $name>]($state.get_param($id.into()).to_f32());
        }
    };

    // -- bool --
    (@wire $state:expr, $ui:expr, $id:expr, $name:ident : bool) => {
        {
            let s = $state.clone();
            let id: u32 = $id.into();
            $crate::paste! {
                $ui.[<on_ $name _changed>](move |v: bool| {
                    s.automate(id, if v { 1.0 } else { 0.0 });
                });
            }
        }
    };
    (@sync $state:expr, $ui:expr, $id:expr, $name:ident : bool) => {
        $crate::paste! {
            $ui.[<set_ $name>]($state.get_param($id.into()) > 0.5);
        }
    };

    // -- choice (integer index for ComboBox / enum params) --
    //
    // Binds an integer property (e.g. ComboBox `current-index`) to an enum
    // param. `count` is the number of options.
    //
    // ```ignore
    // truce_slint::bind! { state, ui,
    //     P::Mode => mode: choice(3),
    // }
    // ```
    (@wire $state:expr, $ui:expr, $id:expr, $name:ident : choice($count:expr)) => {
        {
            let s = $state.clone();
            let id: u32 = $id.into();
            let count: u32 = $count;
            $crate::paste! {
                $ui.[<on_ $name _changed>](move |v: i32| {
                    let norm = $crate::truce_core::cast::discrete_norm(v.max(0) as usize, count as usize);
                    s.automate(id, norm);
                });
            }
        }
    };
    (@sync $state:expr, $ui:expr, $id:expr, $name:ident : choice($count:expr)) => {
        {
            let count: u32 = $count;
            // `discrete_index` takes `f64`; `.to_f64()` widens
            // uniformly regardless of which prelude routed
            // `get_param`.
            let norm = $state.get_param($id.into()).to_f64();
            let idx = $crate::truce_core::cast::discrete_index(norm, count as usize) as i32;
            $crate::paste! {
                $ui.[<set_ $name>](idx);
            }
        }
    };
}
