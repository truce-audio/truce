//! Builds the per-plug-in-type static [`ARAFactory`] the host retrieves
//! at discovery.

use core::ffi::c_char;
use core::mem::size_of;
use core::ptr;
use std::any::TypeId;
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Mutex, OnceLock};

use crate::abi::{
    ARADocumentControllerHostInstance, ARADocumentControllerInstance, ARADocumentProperties,
    ARAFactory, ARAInterfaceConfiguration, kARAAPIGeneration_2_0_Final,
    kARAAPIGeneration_2_3_Final, kARAFalse,
};
use core::any::Any;
use std::sync::Arc;

use crate::audio_source::{AnalysisProgress, AraAudioSource};
use crate::controller::create_controller;
use crate::plugin::{AraExport, AraPlugin};

/// The ARA factory for plug-in type `P`, built once and cached for the
/// binary's lifetime (the host may hold it that long). The build leaks a
/// few C strings per plug-in type - one-time and required by the ARA
/// contract that these pointers stay valid while the binary is loaded.
pub fn ara_factory<P: AraExport>() -> &'static ARAFactory {
    // Per-monomorphization caching without associated statics (a `static`
    // inside a generic fn is shared across all `P`): key a process-global
    // registry by `TypeId`. Locked only at discovery, never on the audio
    // thread.
    let key = TypeId::of::<P>();
    let mut map = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = map.get(&key) {
        return existing;
    }
    let leaked: &'static ARAFactory = Box::leak(Box::new(build_factory::<P>()));
    map.insert(key, leaked);
    leaked
}

fn registry() -> &'static Mutex<HashMap<TypeId, &'static ARAFactory>> {
    static REGISTRY: OnceLock<Mutex<HashMap<TypeId, &'static ARAFactory>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn build_factory<P: AraExport>() -> ARAFactory {
    let info = P::info();
    ARAFactory {
        struct_size: size_of::<ARAFactory>(),
        lowest_supported_api_generation: kARAAPIGeneration_2_0_Final,
        highest_supported_api_generation: kARAAPIGeneration_2_3_Final,
        // Globally-unique, versioned identity. Derived from the plug-in's
        // CLAP id (already reverse-DNS unique); bump the suffix if the
        // archive format or analysis capabilities ever change.
        factory_id: leak_cstr(&format!("{}.ara.factory.1", info.clap_id)),
        initialize_ara_with_configuration: Some(initialize_ara),
        uninitialize_ara: Some(uninitialize_ara),
        plug_in_name: leak_cstr(info.name),
        manufacturer_name: leak_cstr(info.vendor),
        information_url: leak_cstr(info.url),
        version: leak_cstr(info.version),
        create_document_controller_with_document: Some(create_document_controller::<P>),
        document_archive_id: leak_cstr(&format!("{}.ara.archive.1", info.clap_id)),
        compatible_document_archive_ids_count: 0,
        compatible_document_archive_ids: ptr::null(),
        analyzeable_content_types_count: 0,
        analyzeable_content_types: ptr::null(),
        supported_playback_transformation_flags:
            <P::Ara as AraPlugin>::PLAYBACK_TRANSFORMATION_FLAGS,
        supports_storing_audio_file_chunks: kARAFalse,
    }
}

/// Leak a NUL-terminated copy of `s` for the binary's lifetime. Interior
/// NULs (never present in plug-in metadata) collapse to an empty string.
fn leak_cstr(s: &str) -> *const c_char {
    let owned = CString::new(s).unwrap_or_default();
    Box::leak(owned.into_boxed_c_str()).as_ptr()
}

// --- factory vtable thunks -------------------------------------------------

/// ARA start-up. Phase 1 has no global state to prime, so this is a
/// no-op; the host still calls it exactly once before any other ARA call.
unsafe extern "C" fn initialize_ara(_config: *const ARAInterfaceConfiguration) {}

unsafe extern "C" fn uninitialize_ara() {}

unsafe extern "C" fn create_document_controller<P: AraExport>(
    host_instance: *const ARADocumentControllerHostInstance,
    _properties: *const ARADocumentProperties,
) -> *const ARADocumentControllerInstance {
    create_controller(
        ptr::from_ref(P::ara_factory()),
        host_instance,
        analyze_thunk::<P>,
        store_thunk::<P>,
        restore_thunk::<P>,
    )
}

/// Monomorphized, type-erased entry into the plug-in's analysis. The
/// concrete `Analysis` is boxed so the non-generic controller can hold
/// it; phase 4's renderer downcasts it back to `P::Ara::Analysis`.
fn analyze_thunk<P: AraExport>(
    source: &AraAudioSource<'_>,
    progress: &AnalysisProgress,
) -> Arc<dyn Any + Send + Sync> {
    Arc::new(<P::Ara as AraPlugin>::analyze(source, progress))
}

/// Type-erased serialize of one source's analysis (`AraPlugin::store`).
fn store_thunk<P: AraExport>(analysis: &dyn Any, out: &mut Vec<u8>) {
    if let Some(analysis) = analysis.downcast_ref::<<P::Ara as AraPlugin>::Analysis>() {
        <P::Ara as AraPlugin>::store(analysis, out);
    }
}

/// Type-erased deserialize (`AraPlugin::restore`), producing a fresh
/// analysis Arc ready to republish.
fn restore_thunk<P: AraExport>(bytes: &[u8]) -> Option<Arc<dyn Any + Send + Sync>> {
    <P::Ara as AraPlugin>::restore(bytes)
        .map(|analysis| Arc::new(analysis) as Arc<dyn Any + Send + Sync>)
}

/// Bound plug-in-instance extension. Phase 1 assumes no rendering/editing
/// roles yet, so all role interfaces are null; the host binds the
/// instance but never asks it to render (playback/editor renderers land
/// in phase 4). One shared inert instance is sufficient while it carries
/// no per-instance state.
#[must_use]
pub fn null_plugin_extension() -> *const crate::abi::ARAPlugInExtensionInstance {
    use crate::abi::ARAPlugInExtensionInstance;
    static EXT: ExtHolder = ExtHolder(ARAPlugInExtensionInstance {
        struct_size: size_of::<ARAPlugInExtensionInstance>(),
        plug_in_extension_ref: ptr::null_mut(),
        plug_in_extension_interface: ptr::null(),
        playback_renderer_ref: ptr::null_mut(),
        playback_renderer_interface: ptr::null(),
        editor_renderer_ref: ptr::null_mut(),
        editor_renderer_interface: ptr::null(),
        editor_view_ref: ptr::null_mut(),
        editor_view_interface: ptr::null(),
    });
    ptr::from_ref(&EXT.0)
}

/// Wrapper so the all-null, immutable extension can live in a `static`.
struct ExtHolder(crate::abi::ARAPlugInExtensionInstance);
// SAFETY: `EXT` is immutable and its pointers are all null; sharing it
// across threads is sound (it carries no state in phase 1).
unsafe impl Sync for ExtHolder {}
