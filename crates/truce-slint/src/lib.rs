//! Slint GUI backend for truce audio plugins.
//!
//! Provides `SlintEditor`, which implements `truce_core::Editor` using
//! Slint's software renderer + baseview + wgpu. Developers write their UI
//! in `.slint` markup (compiled at build time) and wire parameters through
//! `ParamState`.
//!
//! # Usage
//!
//! ```ignore
//! use truce_slint::{SlintEditor, ParamState};
//!
//! SlintEditor::new((400, 300), |state: ParamState| {
//!     let ui = MyPluginUi::new().unwrap();
//!     truce_slint::bind! { state, ui,
//!         P::Gain   => gain,
//!         P::Pan    => pan,
//!         P::Bypass => bypass: bool,
//!     }
//! })
//! ```

pub mod blit;
pub mod editor;
pub mod param_state;
pub mod platform;

pub use editor::SlintEditor;
pub use param_state::ParamState;

// Re-export slint so plugin authors can use it without a direct dependency.
pub use slint;

// Re-export paste (used by the bind! macro).
#[doc(hidden)]
pub use paste::paste;

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
    ($state:expr, $ui:expr, $( $id:expr => $name:ident $( : $ty:ident )? ),* $(,)?) => {{
        $(
            $crate::bind!(@wire $state, $ui, $id, $name $( : $ty )?);
        )*
        let ui = $ui;
        Box::new(move |state: &$crate::ParamState| {
            $(
                $crate::bind!(@sync state, ui, $id, $name $( : $ty )?);
            )*
        }) as Box<dyn Fn(&$crate::ParamState)>
    }};

    // -- float (default) --
    (@wire $state:expr, $ui:expr, $id:expr, $name:ident) => {
        {
            let s = $state.clone();
            let id: u32 = $id.into();
            $crate::paste! {
                $ui.[<on_ $name _changed>](move |v| s.set_immediate(id, v as f64));
            }
        }
    };
    (@sync $state:expr, $ui:expr, $id:expr, $name:ident) => {
        $crate::paste! {
            $ui.[<set_ $name>]($state.get($id) as f32);
        }
    };

    // -- bool --
    (@wire $state:expr, $ui:expr, $id:expr, $name:ident : bool) => {
        {
            let s = $state.clone();
            let id: u32 = $id.into();
            $crate::paste! {
                $ui.[<on_ $name _changed>](move |v: bool| {
                    s.set_immediate(id, if v { 1.0 } else { 0.0 });
                });
            }
        }
    };
    (@sync $state:expr, $ui:expr, $id:expr, $name:ident : bool) => {
        $crate::paste! {
            $ui.[<set_ $name>]($state.get($id) > 0.5);
        }
    };
}
