//! VST3 ARA discovery + binding thunks (gated behind the `ara` feature).
//!
//! VST3 exposes ARA two ways, both from `ARAVST3.h`: a second factory
//! class implementing `ARA::IMainFactory` (so a host retrieves the
//! `ARAFactory` at scan time without instantiating the processor), and
//! `ARA::IPlugInEntryPoint2` on the audio-processor component (so the host
//! binds an instance to a document controller). The C++ shim answers both
//! by calling these monomorphized thunks, which forward to `truce_ara`
//! exactly as the CLAP and AU wrappers do.

use core::ffi::c_void;
use core::ptr;

use truce_ara::AraExport;
use truce_ara::abi::ARAPlugInInstanceRoleFlags;

use crate::Vst3Instance;

/// Return the plug-in type's static `ARAFactory` as an opaque pointer. The
/// factory is per plug-in *type*, so the per-instance `ctx` is unused (and
/// never dereferenced, so this stays safe).
pub extern "C" fn ara_factory_thunk<P: AraExport>(_ctx: *mut c_void) -> *const c_void {
    ptr::from_ref(P::ara_factory()).cast::<c_void>()
}

/// Bind this instance to the host's ARA document controller and return the
/// `ARAPlugInExtensionInstance` pointer. Mirrors the CLAP bind path: create
/// the playback binding for the assigned roles, store it on the instance
/// (freeing any prior one), and hand the host the instance pointer.
///
/// # Safety
/// `ctx` must be this instance's live `Vst3Instance<P>` pointer (the ctx
/// created in `cb_create`); `document_controller` is the host's opaque ARA
/// controller ref. Called by the shim on the host thread before activate.
pub unsafe extern "C" fn ara_bind_thunk<P: AraExport>(
    ctx: *mut c_void,
    document_controller: *const c_void,
    _known_roles: ARAPlugInInstanceRoleFlags,
    assigned_roles: ARAPlugInInstanceRoleFlags,
) -> *const c_void {
    if ctx.is_null() {
        return ptr::null();
    }
    // SAFETY: `ctx` is this instance's `Vst3Instance<P>` (the shim ctx).
    let inst = unsafe { &*ctx.cast::<Vst3Instance<P>>() };
    // This instance's params, type-erased so an editor-view instance can
    // publish them as the document's shared set.
    let params: std::sync::Arc<dyn core::any::Any + Send + Sync> = inst.params_arc.clone();
    // SAFETY: `document_controller` is the host's ARA controller ref for
    // our document (our `DocumentController` pointer), or null.
    let binding = unsafe {
        truce_ara::create_playback_binding(document_controller.cast_mut(), assigned_roles, params)
    };
    inst.ara_binding.store(binding);
    // SAFETY: `binding` is the live binding just created above.
    unsafe { truce_ara::binding_instance(binding) }.cast::<c_void>()
}
