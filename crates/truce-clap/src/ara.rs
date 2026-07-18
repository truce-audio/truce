//! CLAP ARA discovery glue (gated behind the `ara` feature).
//!
//! Two CLAP extensions from `ARACLAP.h` let an ARA host reach the ARA
//! factory both at the entry (factory) level and per plug-in instance:
//!
//! - `clap_ara_factory` (retrieved via `clap_plugin_entry.get_factory`
//!   with [`CLAP_EXT_ARA_FACTORY`]) hands the host the static
//!   [`ARAFactory`] independent of any instance.
//! - `clap_ara_plugin_extension` (retrieved via
//!   `clap_plugin.get_extension` with [`CLAP_EXT_ARA_PLUGINEXTENSION`])
//!   exposes the same factory per instance plus the bind call that
//!   couples the instance to a document controller.
//!
//! Both vtables are per-plug-in-type statics built with the same
//! monomorphized-thunk pattern the rest of truce uses; the ARA types
//! come from `truce-ara`.

use core::ffi::{CStr, c_char, c_void};
use core::marker::PhantomData;
use core::ptr;
use std::any::TypeId;
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Mutex, OnceLock};

use clap_sys::plugin::clap_plugin;
use truce_ara::AraExport;
use truce_ara::abi::{
    ARADocumentControllerRef, ARAFactory, ARAPlugInExtensionInstance, ARAPlugInInstanceRoleFlags,
};
use truce_core::export::PluginExport;

/// Factory id for `clap_plugin_entry.get_factory` (ARA 2).
pub const CLAP_EXT_ARA_FACTORY: &CStr = c"org.ara-audio.ara.factory/2";
/// Extension id for `clap_plugin.get_extension` (ARA 2).
pub const CLAP_EXT_ARA_PLUGINEXTENSION: &CStr = c"org.ara-audio.ara.pluginextension/2";
/// Descriptor feature advertised when the plug-in supports ARA.
pub const CLAP_PLUGIN_FEATURE_ARA_SUPPORTED: &CStr = c"ara:supported";

// --- clap_ara_factory (ARACLAP.h) ------------------------------------------

#[repr(C)]
pub struct clap_ara_factory {
    pub get_factory_count: Option<unsafe extern "C" fn(*const clap_ara_factory) -> u32>,
    pub get_ara_factory:
        Option<unsafe extern "C" fn(*const clap_ara_factory, u32) -> *const ARAFactory>,
    pub get_plugin_id: Option<unsafe extern "C" fn(*const clap_ara_factory, u32) -> *const c_char>,
}
// SAFETY: an immutable `'static` vtable of function pointers; the ARA/CLAP
// contract requires it to outlive the binary and be shareable.
unsafe impl Sync for clap_ara_factory {}

/// The entry-level ARA factory for plug-in type `P`.
#[must_use]
pub fn clap_ara_factory<P: AraExport>() -> *const clap_ara_factory {
    struct Holder<P>(PhantomData<P>);
    impl<P: AraExport> Holder<P> {
        const F: clap_ara_factory = clap_ara_factory {
            get_factory_count: Some(get_factory_count),
            get_ara_factory: Some(get_ara_factory::<P>),
            get_plugin_id: Some(get_plugin_id::<P>),
        };
    }
    ptr::from_ref(&Holder::<P>::F)
}

unsafe extern "C" fn get_factory_count(_factory: *const clap_ara_factory) -> u32 {
    // One ARA-capable plug-in per binary in the truce model.
    1
}

unsafe extern "C" fn get_ara_factory<P: AraExport>(
    _factory: *const clap_ara_factory,
    index: u32,
) -> *const ARAFactory {
    if index == 0 {
        ptr::from_ref(P::ara_factory())
    } else {
        ptr::null()
    }
}

unsafe extern "C" fn get_plugin_id<P: AraExport>(
    _factory: *const clap_ara_factory,
    index: u32,
) -> *const c_char {
    if index == 0 {
        plugin_id_cstr::<P>()
    } else {
        ptr::null()
    }
}

// --- clap_ara_plugin_extension (ARACLAP.h) ---------------------------------

#[repr(C)]
pub struct clap_ara_plugin_extension {
    pub get_factory: Option<unsafe extern "C" fn(*const clap_plugin) -> *const ARAFactory>,
    pub bind_to_document_controller: Option<
        unsafe extern "C" fn(
            *const clap_plugin,
            ARADocumentControllerRef,
            ARAPlugInInstanceRoleFlags,
            ARAPlugInInstanceRoleFlags,
        ) -> *const ARAPlugInExtensionInstance,
    >,
}
// SAFETY: as for `clap_ara_factory` - immutable `'static` vtable.
unsafe impl Sync for clap_ara_plugin_extension {}

/// The per-instance ARA plug-in extension for plug-in type `P`.
#[must_use]
pub fn clap_ara_plugin_extension<P: AraExport>() -> *const clap_ara_plugin_extension {
    struct Holder<P>(PhantomData<P>);
    impl<P: AraExport> Holder<P> {
        const E: clap_ara_plugin_extension = clap_ara_plugin_extension {
            get_factory: Some(ext_get_factory::<P>),
            bind_to_document_controller: Some(bind_to_document_controller::<P>),
        };
    }
    ptr::from_ref(&Holder::<P>::E)
}

unsafe extern "C" fn ext_get_factory<P: AraExport>(
    _plugin: *const clap_plugin,
) -> *const ARAFactory {
    ptr::from_ref(P::ara_factory())
}

unsafe extern "C" fn bind_to_document_controller<P: AraExport>(
    plugin: *const clap_plugin,
    controller: ARADocumentControllerRef,
    _known_roles: ARAPlugInInstanceRoleFlags,
    assigned_roles: ARAPlugInInstanceRoleFlags,
) -> *const ARAPlugInExtensionInstance {
    // Create the real playback-renderer binding for the assigned roles and
    // store it on the instance so it's freed when the instance is
    // destroyed.
    // SAFETY: `plugin` is our instance.
    let inst = unsafe { crate::data_from_plugin::<P>(plugin) };
    // This instance's params, type-erased so an editor-view instance can
    // publish them as the document's shared set.
    let params: std::sync::Arc<dyn core::any::Any + Send + Sync> = inst.params_arc.clone();
    // SAFETY: `controller` is the host's ARA controller ref for our
    // document (our `DocumentController` pointer), or null.
    let binding =
        unsafe { truce_ara::create_playback_binding(controller.cast(), assigned_roles, params) };
    // Storing swaps out and frees any prior binding (ARA binds at most
    // once, so there is none).
    inst.ara_binding.store(binding);
    // SAFETY: `binding` is a live binding just created above.
    unsafe { truce_ara::binding_instance(binding) }
}

/// Type-erased ARA extension resolver stored on each CLAP instance. The
/// generic `clap_plugin.get_extension` calls it for ids it doesn't
/// recognize, which lets the ARA branch (bounded on `AraExport`) live
/// behind a plain function pointer instead of leaking the bound onto the
/// non-ARA code path.
pub type AraExtResolver = unsafe extern "C" fn(*const clap_plugin, *const c_char) -> *const c_void;

/// Resolve the ARA plug-in extension by id. Installed as the instance's
/// [`AraExtResolver`] by `export_clap!` when the `ara` feature is on.
#[must_use]
pub fn resolver<P: AraExport>() -> AraExtResolver {
    resolve_extension::<P>
}

unsafe extern "C" fn resolve_extension<P: AraExport>(
    _plugin: *const clap_plugin,
    id: *const c_char,
) -> *const c_void {
    if id.is_null() {
        return ptr::null();
    }
    // SAFETY: the host passes a valid NUL-terminated extension id.
    let ext_id = unsafe { CStr::from_ptr(id) };
    if ext_id == CLAP_EXT_ARA_PLUGINEXTENSION {
        return clap_ara_plugin_extension::<P>().cast();
    }
    ptr::null()
}

/// Leak (once per plug-in type) a NUL-terminated copy of the plug-in's
/// CLAP id for `get_plugin_id`. The ARA/CLAP contract requires the
/// returned pointer to stay valid until `clap_plugin_entry.deinit`.
fn plugin_id_cstr<P: PluginExport>() -> *const c_char {
    static IDS: OnceLock<Mutex<HashMap<TypeId, &'static CStr>>> = OnceLock::new();
    let ids = IDS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = ids
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = TypeId::of::<P>();
    if let Some(existing) = map.get(&key) {
        return existing.as_ptr();
    }
    let owned = CString::new(P::info().clap_id).unwrap_or_default();
    let leaked: &'static CStr = Box::leak(owned.into_boxed_c_str());
    map.insert(key, leaked);
    leaked.as_ptr()
}
