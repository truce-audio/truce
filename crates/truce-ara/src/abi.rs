//! Hand-written `#[repr(C)]` bindings for the ARA C ABI.
//!
//! Transcribed from `ARAInterface.h` / `ARACLAP.h` (ARA API 2.3,
//! Apache-2.0, Celemony). The ARA headers pack their
//! structs with `#pragma pack(push, 8)`; on every truce target
//! (`x86_64` / `aarch64`) no field exceeds 8-byte alignment, so plain
//! `#[repr(C)]` reproduces the layout exactly. Vtable slots are
//! `Option<extern "C" fn>` so a null slot round-trips a C `NULL` and a
//! plug-in can leave later (higher `structSize`) slots unimplemented.
//!
//! Only the surface phase 1 needs (factory, plug-in extension, the
//! document-controller vtable) carries precise pointee types; model
//! objects the stub controller never dereferences are typed as opaque
//! `*const c_void`, which is ABI-identical to their real pointer type.

#![allow(non_camel_case_types, non_upper_case_globals)]

use core::ffi::{c_char, c_void};

// --- Scalars (ARAInterface.h base typedefs) --------------------------------

pub type ARAByte = u8;
pub type ARAInt32 = i32;
pub type ARAInt64 = i64;
pub type ARASize = usize;
/// ARA booleans are `ARAInt32`, not C `bool`.
pub type ARABool = i32;
pub type ARAUtf8String = *const c_char;
pub type ARAPersistentID = *const c_char;
pub type ARASampleRate = f64;
pub type ARASampleCount = i64;
pub type ARAChannelCount = i32;
pub type ARATimePosition = f64;
pub type ARATimeDuration = f64;
/// `ARA_32_BIT_ENUM` types are all `int32_t`-sized.
pub type ARAAPIGeneration = i32;
pub type ARAContentType = i32;
pub type ARAContentGrade = i32;
pub type ARAContentUpdateFlags = i32;
pub type ARAPlaybackTransformationFlags = i32;
pub type ARAPlugInInstanceRoleFlags = i32;

pub const kARAFalse: ARABool = 0;
pub const kARATrue: ARABool = 1;

// API generations we care about (ARAInterface.h).
pub const kARAAPIGeneration_2_0_Final: ARAAPIGeneration = 4;
pub const kARAAPIGeneration_2_3_Final: ARAAPIGeneration = 6;

// Plug-in instance role flags (ARAInterface.h).
pub const kARAPlaybackRendererRole: ARAPlugInInstanceRoleFlags = 1 << 0;
pub const kARAEditorRendererRole: ARAPlugInInstanceRoleFlags = 1 << 1;
pub const kARAEditorViewRole: ARAPlugInInstanceRoleFlags = 1 << 2;

// Playback transformation flags (ARAInterface.h). A factory declaring any
// non-`NoChanges` flag tells the host it modifies playback, so the host
// engages it as a playback renderer rather than optimizing it away.
pub const kARAPlaybackTransformationNoChanges: ARAPlaybackTransformationFlags = 0;
pub const kARAPlaybackTransformationTimestretch: ARAPlaybackTransformationFlags = 1 << 0;
pub const kARAPlaybackTransformationTimestretchReflectingTempo: ARAPlaybackTransformationFlags =
    1 << 1;
pub const kARAPlaybackTransformationContentBasedFadeAtTail: ARAPlaybackTransformationFlags = 1 << 2;
pub const kARAPlaybackTransformationContentBasedFadeAtHead: ARAPlaybackTransformationFlags = 1 << 3;

// --- Opaque reference types (ARA_REF / ARA_HOST_REF) -----------------------

/// Define an opaque, pointer-sized ARA reference type. `ARA_REF(X)`
/// expands to `struct XMarkupType * X`, so each `X` is a distinct raw
/// pointer to an incomplete type - reproduced here as a zero-field
/// marker struct plus a `*mut` alias.
macro_rules! ara_ref {
    ($($name:ident),+ $(,)?) => {$(
        paste_ref!($name);
    )+};
}
macro_rules! paste_ref {
    ($name:ident) => {
        #[doc = concat!("Opaque ARA reference `", stringify!($name), "`.")]
        pub type $name = *mut c_void;
    };
}

ara_ref!(
    // Plug-in-side refs the plug-in hands back to the host.
    ARADocumentControllerRef,
    ARAMusicalContextRef,
    ARAAudioSourceRef,
    ARAAudioModificationRef,
    ARAPlaybackRegionRef,
    ARAContentReaderRef,
    ARARegionSequenceRef,
    ARAPlaybackRendererRef,
    ARAEditorRendererRef,
    ARAEditorViewRef,
    ARAPlugInExtensionRef,
    // Host-side refs the plug-in passes back when calling host callbacks.
    ARAMusicalContextHostRef,
    ARAAudioSourceHostRef,
    ARAAudioModificationHostRef,
    ARAPlaybackRegionHostRef,
    ARARegionSequenceHostRef,
    ARAArchiveReaderHostRef,
    ARAArchiveWriterHostRef,
    ARAAudioAccessControllerHostRef,
    ARAAudioReaderHostRef,
    ARAModelUpdateControllerHostRef,
    ARAArchivingControllerHostRef,
);

/// Host callback vtable for stream-based archive I/O (phase 5). The
/// plug-in reads/writes its persistent state (per-source analysis) at
/// byte positions in the host-managed archive.
#[repr(C)]
pub struct ARAArchivingControllerInterface {
    pub struct_size: ARASize,
    pub get_archive_size: Option<
        unsafe extern "C" fn(ARAArchivingControllerHostRef, ARAArchiveReaderHostRef) -> ARASize,
    >,
    pub read_bytes_from_archive: Option<
        unsafe extern "C" fn(
            ARAArchivingControllerHostRef,
            ARAArchiveReaderHostRef,
            ARASize, // position
            ARASize, // length
            *mut ARAByte,
        ) -> ARABool,
    >,
    pub write_bytes_to_archive: Option<
        unsafe extern "C" fn(
            ARAArchivingControllerHostRef,
            ARAArchiveWriterHostRef,
            ARASize, // position
            ARASize, // length
            *const ARAByte,
        ) -> ARABool,
    >,
    pub notify_document_archiving_progress:
        Option<unsafe extern "C" fn(ARAArchivingControllerHostRef, f32)>,
    pub notify_document_unarchiving_progress:
        Option<unsafe extern "C" fn(ARAArchivingControllerHostRef, f32)>,
}

// --- host-provided controller interfaces (phase 3) -------------------------

pub type ARAAnalysisProgressState = i32;
pub const kARAAnalysisProgressStarted: ARAAnalysisProgressState = 0;
pub const kARAAnalysisProgressUpdated: ARAAnalysisProgressState = 1;
pub const kARAAnalysisProgressCompleted: ARAAnalysisProgressState = 2;

pub const kARAContentGradeInitial: ARAContentGrade = 0;
pub const kARAContentGradeDetected: ARAContentGrade = 1;

/// Host callback vtable for random audio access. The plug-in creates a
/// long-lived reader per source, reads non-interleaved samples off the
/// audio thread, and destroys the reader when analysis finishes.
#[repr(C)]
pub struct ARAAudioAccessControllerInterface {
    pub struct_size: ARASize,
    pub create_audio_reader_for_source: Option<
        unsafe extern "C" fn(
            ARAAudioAccessControllerHostRef,
            ARAAudioSourceHostRef,
            ARABool,
        ) -> ARAAudioReaderHostRef,
    >,
    pub read_audio_samples: Option<
        unsafe extern "C" fn(
            ARAAudioAccessControllerHostRef,
            ARAAudioReaderHostRef,
            ARASampleCount, // sample position
            ARASampleCount, // samples per channel
            *const *mut core::ffi::c_void,
        ) -> ARABool,
    >,
    pub destroy_audio_reader:
        Option<unsafe extern "C" fn(ARAAudioAccessControllerHostRef, ARAAudioReaderHostRef)>,
}

/// Plug-in-side playback-renderer vtable (phase 4). The host assigns
/// playback regions to a renderer instance while it's inactive; the
/// renderer generates their audio during `process`.
#[repr(C)]
pub struct ARAPlaybackRendererInterface {
    pub struct_size: ARASize,
    pub add_playback_region:
        Option<unsafe extern "C" fn(ARAPlaybackRendererRef, ARAPlaybackRegionRef)>,
    pub remove_playback_region:
        Option<unsafe extern "C" fn(ARAPlaybackRendererRef, ARAPlaybackRegionRef)>,
}

/// The editor renderer (ARA 2.0): previews the regions being edited. Same
/// region-assignment shape as the playback renderer, plus region-sequence
/// shortcuts. Transcribed from `ARAInterface.h`.
#[repr(C)]
pub struct ARAEditorRendererInterface {
    pub struct_size: ARASize,
    pub add_playback_region:
        Option<unsafe extern "C" fn(ARAEditorRendererRef, ARAPlaybackRegionRef)>,
    pub remove_playback_region:
        Option<unsafe extern "C" fn(ARAEditorRendererRef, ARAPlaybackRegionRef)>,
    pub add_region_sequence:
        Option<unsafe extern "C" fn(ARAEditorRendererRef, ARARegionSequenceRef)>,
    pub remove_region_sequence:
        Option<unsafe extern "C" fn(ARAEditorRendererRef, ARARegionSequenceRef)>,
}

/// The editor view (ARA 2.0): host -> plug-in UI notifications (selection,
/// hidden region sequences). A plug-in with no ARA-aware UI may ignore
/// them. `ARAViewSelection` is opaque here (`*const c_void`).
#[repr(C)]
pub struct ARAEditorViewInterface {
    pub struct_size: ARASize,
    pub notify_selection: Option<unsafe extern "C" fn(ARAEditorViewRef, *const c_void)>,
    pub notify_hide_region_sequences:
        Option<unsafe extern "C" fn(ARAEditorViewRef, ARASize, *const ARARegionSequenceRef)>,
}

/// Host callback vtable for plug-in -> host model-update notifications
/// (only the analysis-progress + content-changed prefix is bound here).
#[repr(C)]
pub struct ARAModelUpdateControllerInterface {
    pub struct_size: ARASize,
    pub notify_audio_source_analysis_progress: Option<
        unsafe extern "C" fn(
            ARAModelUpdateControllerHostRef,
            ARAAudioSourceHostRef,
            ARAAnalysisProgressState,
            f32,
        ),
    >,
    pub notify_audio_source_content_changed: Option<
        unsafe extern "C" fn(
            ARAModelUpdateControllerHostRef,
            ARAAudioSourceHostRef,
            *const core::ffi::c_void,
            ARAContentUpdateFlags,
        ),
    >,
    pub notify_audio_modification_content_changed: Option<
        unsafe extern "C" fn(
            ARAModelUpdateControllerHostRef,
            ARAAudioModificationHostRef,
            *const core::ffi::c_void,
            ARAContentUpdateFlags,
        ),
    >,
}

// --- Small structs ---------------------------------------------------------

/// `void (*)(ARAAssertCategory, const void *, ARAUtf8String)`.
pub type ARAAssertFunction =
    Option<unsafe extern "C" fn(category: i32, arg: *const c_void, diagnosis: *const c_char)>;

#[repr(C)]
pub struct ARADocumentProperties {
    pub struct_size: ARASize,
    pub name: ARAUtf8String,
}

/// RGB color a host attaches to model objects (all components 0..=1).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ARAColor {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

/// Model-object property structs. Every field past `struct_size`'s
/// mandatory minimum is an addendum: read it only when
/// `struct_size` covers it (see `controller`'s `field` helper). Layouts
/// match `ARAInterface.h` under `#[repr(C)]`.
#[repr(C)]
pub struct ARAMusicalContextProperties {
    pub struct_size: ARASize,
    pub name: ARAUtf8String,
    pub order_index: ARAInt32,
    pub color: *const ARAColor,
}

#[repr(C)]
pub struct ARARegionSequenceProperties {
    pub struct_size: ARASize,
    pub name: ARAUtf8String,
    pub order_index: ARAInt32,
    pub musical_context_ref: ARAMusicalContextRef,
    pub color: *const ARAColor,
}

#[repr(C)]
pub struct ARAAudioSourceProperties {
    pub struct_size: ARASize,
    pub name: ARAUtf8String,
    pub persistent_id: ARAPersistentID,
    pub sample_count: ARASampleCount,
    pub sample_rate: ARASampleRate,
    pub channel_count: ARAChannelCount,
    pub merits_64bit_samples: ARABool,
    pub channel_arrangement_data_type: ARAInt32,
    pub channel_arrangement: *const core::ffi::c_void,
}

#[repr(C)]
pub struct ARAAudioModificationProperties {
    pub struct_size: ARASize,
    pub name: ARAUtf8String,
    pub persistent_id: ARAPersistentID,
}

#[repr(C)]
pub struct ARAPlaybackRegionProperties {
    pub struct_size: ARASize,
    pub transformation_flags: ARAPlaybackTransformationFlags,
    pub start_in_modification_time: ARATimePosition,
    pub duration_in_modification_time: ARATimeDuration,
    pub start_in_playback_time: ARATimePosition,
    pub duration_in_playback_time: ARATimeDuration,
    pub musical_context_ref: ARAMusicalContextRef,
    pub region_sequence_ref: ARARegionSequenceRef,
    pub name: ARAUtf8String,
    pub color: *const ARAColor,
}

#[repr(C)]
pub struct ARAInterfaceConfiguration {
    pub struct_size: ARASize,
    pub desired_api_generation: ARAAPIGeneration,
    /// Pointer to a function pointer (the host's assert hook), or null.
    pub assert_function_address: *const ARAAssertFunction,
}

/// Host callback bundle handed to `createDocumentControllerWithDocument`.
/// The optional interfaces are opaque here (phase 1 never calls them);
/// each field pair is `(hostRef, *const interface)`, all pointer-sized.
#[repr(C)]
pub struct ARADocumentControllerHostInstance {
    pub struct_size: ARASize,
    pub audio_access_controller_host_ref: ARAAudioAccessControllerHostRef,
    pub audio_access_controller_interface: *const ARAAudioAccessControllerInterface,
    pub archiving_controller_host_ref: ARAArchivingControllerHostRef,
    pub archiving_controller_interface: *const ARAArchivingControllerInterface,
    pub content_access_controller_host_ref: *mut c_void,
    pub content_access_controller_interface: *const c_void,
    pub model_update_controller_host_ref: ARAModelUpdateControllerHostRef,
    pub model_update_controller_interface: *const ARAModelUpdateControllerInterface,
    pub playback_controller_host_ref: *mut c_void,
    pub playback_controller_interface: *const c_void,
}

#[repr(C)]
pub struct ARADocumentControllerInstance {
    pub struct_size: ARASize,
    pub document_controller_ref: ARADocumentControllerRef,
    pub document_controller_interface: *const ARADocumentControllerInterface,
}

/// Bound plug-in instance's ARA-specific interfaces. All roles null in
/// phase 1 (rendering/editing arrive in later phases); the host reads
/// `struct_size` to know which slots exist.
#[repr(C)]
pub struct ARAPlugInExtensionInstance {
    pub struct_size: ARASize,
    // Deprecated ARA-1 combined interface (kept for layout, always null).
    pub plug_in_extension_ref: ARAPlugInExtensionRef,
    pub plug_in_extension_interface: *const c_void,
    // ARA-2 per-role interfaces.
    pub playback_renderer_ref: ARAPlaybackRendererRef,
    pub playback_renderer_interface: *const c_void,
    pub editor_renderer_ref: ARAEditorRendererRef,
    pub editor_renderer_interface: *const c_void,
    pub editor_view_ref: ARAEditorViewRef,
    pub editor_view_interface: *const c_void,
}

// --- The document-controller vtable ----------------------------------------

/// The central plug-in-side ARA vtable. Field order and signatures match
/// `ARADocumentControllerInterface` exactly. Pointee types the stub never
/// dereferences (properties / content / archive structs) are `*const
/// c_void` - ABI-identical to the real `const ARA*Properties *`.
#[repr(C)]
pub struct ARADocumentControllerInterface {
    pub struct_size: ARASize,

    // Destruction + factory back-link.
    pub destroy_document_controller: Option<unsafe extern "C" fn(ARADocumentControllerRef)>,
    pub get_factory: Option<unsafe extern "C" fn(ARADocumentControllerRef) -> *const ARAFactory>,

    // Editing cycle + model-update pump.
    pub begin_editing: Option<unsafe extern "C" fn(ARADocumentControllerRef)>,
    pub end_editing: Option<unsafe extern "C" fn(ARADocumentControllerRef)>,
    pub notify_model_updates: Option<unsafe extern "C" fn(ARADocumentControllerRef)>,

    // Deprecated ARA-1 whole-document archiving.
    pub begin_restoring_document_from_archive:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAArchiveReaderHostRef) -> ARABool>,
    pub end_restoring_document_from_archive:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAArchiveReaderHostRef) -> ARABool>,
    pub store_document_to_archive:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAArchiveWriterHostRef) -> ARABool>,

    pub update_document_properties:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, *const c_void)>,

    // Musical contexts.
    pub create_musical_context: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAMusicalContextHostRef,
            *const c_void,
        ) -> ARAMusicalContextRef,
    >,
    pub update_musical_context_properties:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAMusicalContextRef, *const c_void)>,
    pub update_musical_context_content: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAMusicalContextRef,
            *const c_void,
            ARAContentUpdateFlags,
        ),
    >,
    pub destroy_musical_context:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAMusicalContextRef)>,

    // Audio sources.
    pub create_audio_source: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAAudioSourceHostRef,
            *const c_void,
        ) -> ARAAudioSourceRef,
    >,
    pub update_audio_source_properties:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAAudioSourceRef, *const c_void)>,
    pub update_audio_source_content: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAAudioSourceRef,
            *const c_void,
            ARAContentUpdateFlags,
        ),
    >,
    pub enable_audio_source_samples_access:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAAudioSourceRef, ARABool)>,
    pub deactivate_audio_source_for_undo_history:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAAudioSourceRef, ARABool)>,
    pub destroy_audio_source:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAAudioSourceRef)>,

    // Audio modifications.
    pub create_audio_modification: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAAudioSourceRef,
            ARAAudioModificationHostRef,
            *const c_void,
        ) -> ARAAudioModificationRef,
    >,
    pub clone_audio_modification: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAAudioModificationRef,
            ARAAudioModificationHostRef,
            *const c_void,
        ) -> ARAAudioModificationRef,
    >,
    pub update_audio_modification_properties: Option<
        unsafe extern "C" fn(ARADocumentControllerRef, ARAAudioModificationRef, *const c_void),
    >,
    pub deactivate_audio_modification_for_undo_history:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAAudioModificationRef, ARABool)>,
    pub destroy_audio_modification:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAAudioModificationRef)>,

    // Playback regions.
    pub create_playback_region: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAAudioModificationRef,
            ARAPlaybackRegionHostRef,
            *const c_void,
        ) -> ARAPlaybackRegionRef,
    >,
    pub update_playback_region_properties:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAPlaybackRegionRef, *const c_void)>,
    pub destroy_playback_region:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAPlaybackRegionRef)>,

    // Content readers (audio source / modification / region).
    pub is_audio_source_content_available: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAAudioSourceRef,
            ARAContentType,
        ) -> ARABool,
    >,
    pub is_audio_source_content_analysis_incomplete: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAAudioSourceRef,
            ARAContentType,
        ) -> ARABool,
    >,
    pub request_audio_source_content_analysis: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAAudioSourceRef,
            ARASize,
            *const ARAContentType,
        ),
    >,
    pub get_audio_source_content_grade: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAAudioSourceRef,
            ARAContentType,
        ) -> ARAContentGrade,
    >,
    pub create_audio_source_content_reader: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAAudioSourceRef,
            ARAContentType,
            *const c_void,
        ) -> ARAContentReaderRef,
    >,
    pub is_audio_modification_content_available: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAAudioModificationRef,
            ARAContentType,
        ) -> ARABool,
    >,
    pub get_audio_modification_content_grade: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAAudioModificationRef,
            ARAContentType,
        ) -> ARAContentGrade,
    >,
    pub create_audio_modification_content_reader: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAAudioModificationRef,
            ARAContentType,
            *const c_void,
        ) -> ARAContentReaderRef,
    >,
    pub is_playback_region_content_available: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAPlaybackRegionRef,
            ARAContentType,
        ) -> ARABool,
    >,
    pub get_playback_region_content_grade: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAPlaybackRegionRef,
            ARAContentType,
        ) -> ARAContentGrade,
    >,
    pub create_playback_region_content_reader: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAPlaybackRegionRef,
            ARAContentType,
            *const c_void,
        ) -> ARAContentReaderRef,
    >,
    pub get_content_reader_event_count:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAContentReaderRef) -> ARAInt32>,
    pub get_content_reader_data_for_event: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAContentReaderRef,
            ARAInt32,
        ) -> *const c_void,
    >,
    pub destroy_content_reader:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAContentReaderRef)>,
    // --- end of the ARA-1 mandatory prefix (kARADocumentControllerInterfaceMinSize) ---

    // ARA-2 addenda. Left null in the stub; `struct_size` is set to end at
    // `destroy_content_reader` so the host never reads past what we fill.
    pub create_region_sequence: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARARegionSequenceHostRef,
            *const c_void,
        ) -> ARARegionSequenceRef,
    >,
    pub update_region_sequence_properties:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARARegionSequenceRef, *const c_void)>,
    pub destroy_region_sequence:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARARegionSequenceRef)>,
    pub get_playback_region_head_and_tail_time: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAPlaybackRegionRef,
            *mut ARATimeDuration,
            *mut ARATimeDuration,
        ),
    >,
    pub restore_objects_from_archive: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAArchiveReaderHostRef,
            *const c_void,
        ) -> ARABool,
    >,
    pub store_objects_to_archive: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAArchiveWriterHostRef,
            *const c_void,
        ) -> ARABool,
    >,
    pub get_processing_algorithms_count:
        Option<unsafe extern "C" fn(ARADocumentControllerRef) -> ARAInt32>,
    pub get_processing_algorithm_properties:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAInt32) -> *const c_void>,
    pub get_processing_algorithm_for_audio_source:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAAudioSourceRef) -> ARAInt32>,
    pub request_processing_algorithm_for_audio_source:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAAudioSourceRef, ARAInt32)>,
    pub is_licensed_for_capabilities: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARABool,
            ARASize,
            *const ARAContentType,
            ARAPlaybackTransformationFlags,
        ) -> ARABool,
    >,
    pub store_audio_source_to_audio_file_chunk: Option<
        unsafe extern "C" fn(
            ARADocumentControllerRef,
            ARAArchiveWriterHostRef,
            ARAAudioSourceRef,
            *mut ARAPersistentID,
            *mut ARABool,
        ) -> ARABool,
    >,
    pub is_audio_modification_preserving_audio_source_signal:
        Option<unsafe extern "C" fn(ARADocumentControllerRef, ARAAudioModificationRef) -> ARABool>,
}

// --- The static factory ----------------------------------------------------

/// The per-plug-in-type static factory the host retrieves at discovery.
/// Field order matches `ARAFactory`; `supports_storing_audio_file_chunks`
/// is the ARA-2.0-Final tail, included because we advertise generation
/// 2.0 Final or newer.
#[repr(C)]
pub struct ARAFactory {
    pub struct_size: ARASize,
    pub lowest_supported_api_generation: ARAAPIGeneration,
    pub highest_supported_api_generation: ARAAPIGeneration,
    pub factory_id: ARAPersistentID,
    pub initialize_ara_with_configuration:
        Option<unsafe extern "C" fn(*const ARAInterfaceConfiguration)>,
    pub uninitialize_ara: Option<unsafe extern "C" fn()>,
    pub plug_in_name: ARAUtf8String,
    pub manufacturer_name: ARAUtf8String,
    pub information_url: ARAUtf8String,
    pub version: ARAUtf8String,
    pub create_document_controller_with_document: Option<
        unsafe extern "C" fn(
            *const ARADocumentControllerHostInstance,
            *const ARADocumentProperties,
        ) -> *const ARADocumentControllerInstance,
    >,
    pub document_archive_id: ARAPersistentID,
    pub compatible_document_archive_ids_count: ARASize,
    pub compatible_document_archive_ids: *const ARAPersistentID,
    pub analyzeable_content_types_count: ARASize,
    pub analyzeable_content_types: *const ARAContentType,
    pub supported_playback_transformation_flags: ARAPlaybackTransformationFlags,
    pub supports_storing_audio_file_chunks: ARABool,
}

// SAFETY: `ARAFactory` is an immutable, `'static` vtable of function
// pointers and C-string pointers with no interior mutability; the ARA
// contract requires it to remain valid and shareable for the binary's
// lifetime, so it is `Sync`.
unsafe impl Sync for ARAFactory {}
