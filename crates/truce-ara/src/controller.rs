//! The ARA document controller: the model graph (phase 2), offline
//! content analysis (phase 3), and archive persistence (phase 5).
//!
//! The host drives a graph of `MusicalContext` / `RegionSequence` /
//! `AudioSource` -> `AudioModification` -> `PlaybackRegion` objects
//! through create/update/destroy calls, all bracketed by
//! `beginEditing` / `endEditing`. truce stores each object in a
//! generational [`Arena`] and hands the host a [`Handle`]-encoded ref,
//! so a stale ref reads back as "gone" instead of dangling.
//!
//! `requestAudioSourceContentAnalysis` runs the plug-in's `analyze` off
//! the model thread on the shared task pool: the worker reads samples
//! through the host audio-access controller, publishes a type-erased
//! result per source, and reports progress. `store` / `restore` persist
//! each analyzed source's result to the host archive, keyed by
//! `persistentID`, so a re-opened session skips re-analysis. All model
//! access happens on the host's single model thread, so recovering a
//! `&mut DocumentController` from the opaque ref per call is sound.

use core::any::Any;
use core::ffi::{CStr, c_char, c_void};
use core::mem::{offset_of, size_of};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use truce_core::tasks::TaskSpawner;

use crate::abi::{
    ARAArchiveReaderHostRef, ARAArchiveWriterHostRef, ARAArchivingControllerHostRef,
    ARAArchivingControllerInterface, ARAAudioAccessControllerHostRef,
    ARAAudioAccessControllerInterface, ARAAudioModificationProperties, ARAAudioSourceProperties,
    ARABool, ARAContentGrade, ARADocumentControllerHostInstance, ARADocumentControllerInstance,
    ARADocumentControllerInterface, ARADocumentControllerRef, ARAFactory,
    ARAModelUpdateControllerHostRef, ARAModelUpdateControllerInterface,
    ARAMusicalContextProperties, ARAPlaybackRegionProperties, ARARegionSequenceProperties,
    kARAContentGradeDetected, kARAContentGradeInitial, kARAFalse, kARATrue,
};
use crate::archive::{ArchiveReader, ArchiveWriter};
use crate::arena::{Arena, Handle};
use crate::audio_source::{AnalysisProgress, AraAudioSource};

/// A per-plug-in-type type-erased entry into `AraPlugin::analyze`. The
/// concrete `Analysis` is returned as `Arc<dyn Any + Send + Sync>` so the
/// non-generic controller can publish it lock-free; the per-`P` renderer
/// downcasts it back.
pub(crate) type AnalyzeFn =
    fn(&AraAudioSource<'_>, &AnalysisProgress) -> Arc<dyn Any + Send + Sync>;

/// Host audio-access callback bundle (a ref + its vtable), copied into an
/// analysis job so a pool worker can read source samples.
#[derive(Clone, Copy)]
struct HostAudioAccess {
    host_ref: ARAAudioAccessControllerHostRef,
    iface: *const ARAAudioAccessControllerInterface,
}

/// Host model-update callback bundle for reporting analysis progress.
#[derive(Clone, Copy)]
struct HostModelUpdate {
    host_ref: ARAModelUpdateControllerHostRef,
    iface: *const ARAModelUpdateControllerInterface,
}

/// Host archiving callback bundle for persistence (phase 5).
#[derive(Clone, Copy)]
struct HostArchiving {
    host_ref: ARAArchivingControllerHostRef,
    iface: *const ARAArchivingControllerInterface,
}

/// Type-erased serialize of a source's analysis (`AraPlugin::store`).
pub(crate) type StoreFn = fn(&dyn Any, &mut Vec<u8>);
/// Type-erased deserialize (`AraPlugin::restore`), producing a fresh
/// analysis Arc ready to republish.
pub(crate) type RestoreFn = fn(&[u8]) -> Option<Arc<dyn Any + Send + Sync>>;

/// Sized wrapper so the type-erased, unsized `Arc<dyn Any>` can live in an
/// `ArcSwapOption` (which stores only `Sized` pointees). Reused for both
/// the per-source analysis slot and the per-document shared params slot.
pub(crate) struct Published(pub(crate) Arc<dyn Any + Send + Sync>);

/// Shared per-source analysis state. Written by the pool worker, read by
/// the host's model thread (the `analyzed` flag) and by every bound
/// renderer's audio thread (the published result, lock-free).
pub(crate) struct AnalysisState {
    analyzed: AtomicBool,
    /// The latest published analysis. A lock-free, multi-reader slot: one
    /// audio source feeds several bound renderers (a playback renderer and
    /// an editor renderer share it), so each `load` reads the result
    /// without removing it. A single-consumer queue would hand the analysis
    /// to whichever instance rendered first and starve the rest.
    slot: ArcSwapOption<Published>,
}

impl AnalysisState {
    fn new() -> Self {
        Self {
            analyzed: AtomicBool::new(false),
            slot: ArcSwapOption::empty(),
        }
    }

    /// Publish a fresh analysis and mark the source analyzed.
    fn publish(&self, analysis: Arc<dyn Any + Send + Sync>) {
        self.slot.store(Some(Arc::new(Published(analysis))));
        self.analyzed.store(true, Ordering::Release);
    }

    /// The latest published analysis, cloning the `Arc`. Wait-free; safe on
    /// the audio thread and readable by any number of renderers.
    pub(crate) fn load(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.slot.load_full().map(|p| Arc::clone(&p.0))
    }

    /// The current analysis for a model-thread reader (archive store).
    fn current(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.load()
    }
}

/// One analysis request handed to the task pool.
struct AnalysisJob {
    access: HostAudioAccess,
    model: HostModelUpdate,
    source_host_ref: *mut c_void,
    channel_count: u32,
    sample_count: i64,
    sample_rate: f64,
    analyze: AnalyzeFn,
    state: Arc<AnalysisState>,
}

// SAFETY: the raw pointers are ARA host callbacks the spec explicitly
// permits invoking from a non-realtime background thread (one thread per
// reader); the job owns everything else it touches.
unsafe impl Send for AnalysisJob {}

/// Run one analysis on the task pool: read samples through the host,
/// call the plug-in's `analyze`, publish the result, and report progress.
/// Takes the job by value because the task pool's handler signature
/// (`Fn(T)`) moves the task into it.
#[allow(clippy::needless_pass_by_value)]
fn run_analysis(job: AnalysisJob) {
    let progress = AnalysisProgress::new(job.model.host_ref, job.model.iface, job.source_host_ref);
    progress.start();
    // SAFETY: `access` is the host's audio-access controller, valid to
    // call from this non-realtime worker; the refs came from the host.
    let source = unsafe {
        AraAudioSource::new(
            job.access.host_ref,
            job.access.iface,
            job.source_host_ref,
            job.channel_count,
            job.sample_count,
            job.sample_rate,
        )
    };
    if let Some(source) = source {
        let result = (job.analyze)(&source, &progress);
        job.state.publish(result);
    } else {
        // Reader unavailable: mark analyzed with no result so the host
        // stops polling; nothing to render or store.
        job.state.analyzed.store(true, Ordering::Release);
    }
    progress.complete();
}

// --- model objects ---------------------------------------------------------

struct MusicalContext {
    // Read in phase 3 to call host model-update callbacks for this object.
    #[allow(dead_code)]
    host_ref: *mut c_void,
    name: Option<String>,
    order_index: i32,
}

struct RegionSequence {
    // Read in phase 3 to call host model-update callbacks for this object.
    #[allow(dead_code)]
    host_ref: *mut c_void,
    name: Option<String>,
    order_index: i32,
    musical_context: Option<Handle>,
}

struct AudioSource {
    host_ref: *mut c_void,
    name: Option<String>,
    persistent_id: Option<String>,
    sample_count: i64,
    sample_rate: f64,
    channel_count: i32,
    samples_access_enabled: bool,
    analysis: Arc<AnalysisState>,
}

struct AudioModification {
    // Read in phase 3 to call host model-update callbacks for this object.
    #[allow(dead_code)]
    host_ref: *mut c_void,
    source: Handle,
    name: Option<String>,
    persistent_id: Option<String>,
}

struct PlaybackRegion {
    // Read in phase 3 to call host model-update callbacks for this object.
    #[allow(dead_code)]
    host_ref: *mut c_void,
    modification: Handle,
    region_sequence: Option<Handle>,
    start_in_playback_time: f64,
    duration_in_playback_time: f64,
    start_in_modification_time: f64,
    duration_in_modification_time: f64,
}

/// The plug-in side of one ARA document. `instance` is the first field
/// so the `ARADocumentControllerRef` (a pointer to this struct) and the
/// returned instance pointer coincide.
#[repr(C)]
pub(crate) struct DocumentController {
    instance: ARADocumentControllerInstance,
    factory: *const ARAFactory,
    editing: bool,
    access: HostAudioAccess,
    model: HostModelUpdate,
    archiving: HostArchiving,
    analyze: AnalyzeFn,
    store: StoreFn,
    restore: RestoreFn,
    spawner: TaskSpawner<AnalysisJob>,
    musical_contexts: Arena<MusicalContext>,
    region_sequences: Arena<RegionSequence>,
    audio_sources: Arena<AudioSource>,
    audio_modifications: Arena<AudioModification>,
    playback_regions: Arena<PlaybackRegion>,
    /// Plug-in state (type-erased `Arc<P::Params>`) shared by every
    /// companion instance bound to this one ARA document. An ARA host
    /// (Cubase) binds the editor-view, editor-renderer, and playback-
    /// renderer roles to *separate* instances, each with its own params;
    /// this slot lets the renderers read - and the editor write - one
    /// shared set, so a knob turned on the visible editor reaches the
    /// instances that actually render. The editor-view instance publishes
    /// its params here at bind; renderers hold an `Arc` to the slot and
    /// read it lock-free each block.
    shared_params: Arc<ArcSwapOption<Published>>,
}

impl DocumentController {
    fn new(
        factory: *const ARAFactory,
        access: HostAudioAccess,
        model: HostModelUpdate,
        archiving: HostArchiving,
        analyze: AnalyzeFn,
        store: StoreFn,
        restore: RestoreFn,
    ) -> Box<Self> {
        Box::new(Self {
            instance: ARADocumentControllerInstance {
                struct_size: size_of::<ARADocumentControllerInstance>(),
                document_controller_ref: ptr::null_mut(),
                document_controller_interface: ptr::from_ref(&VTABLE),
            },
            factory,
            editing: false,
            access,
            model,
            archiving,
            store,
            restore,
            analyze,
            spawner: TaskSpawner::new(run_analysis),
            musical_contexts: Arena::new(),
            region_sequences: Arena::new(),
            audio_sources: Arena::new(),
            audio_modifications: Arena::new(),
            playback_regions: Arena::new(),
            shared_params: Arc::new(ArcSwapOption::empty()),
        })
    }

    /// A handle to this document's shared-params slot, for a renderer to
    /// read lock-free on the audio thread.
    pub(crate) fn shared_params_handle(&self) -> Arc<ArcSwapOption<Published>> {
        Arc::clone(&self.shared_params)
    }

    /// Publish this document's shared plug-in params (the editor-view
    /// instance's, at bind). Last publish wins - there is one editor-view
    /// instance per document, so it is the canonical source.
    pub(crate) fn publish_shared_params(&self, params: Arc<dyn Any + Send + Sync>) {
        self.shared_params.store(Some(Arc::new(Published(params))));
    }
}

/// Allocate a controller and return its instance pointer, valid until
/// `destroy_document_controller`. `analyze` / `store` / `restore` are the
/// plug-in's monomorphized entries; `host_instance` carries the host
/// audio-access, model-update, and archiving controllers.
pub(crate) fn create_controller(
    factory: *const ARAFactory,
    host_instance: *const ARADocumentControllerHostInstance,
    analyze: AnalyzeFn,
    store: StoreFn,
    restore: RestoreFn,
) -> *const ARADocumentControllerInstance {
    // SAFETY: the host passes a valid host-instance for the document's
    // lifetime (or null, handled below).
    let (access, model, archiving) = unsafe { host_controllers(host_instance) };
    let mut boxed =
        DocumentController::new(factory, access, model, archiving, analyze, store, restore);
    let self_ref = ptr::from_mut(&mut *boxed).cast::<c_void>();
    boxed.instance.document_controller_ref = self_ref;
    let leaked = Box::into_raw(boxed);
    // SAFETY: freshly boxed; `instance` (offset 0) lives until
    // `destroy_document_controller` frees the whole allocation.
    unsafe { ptr::from_ref(&(*leaked).instance) }
}

/// Extract the audio-access + model-update + archiving controllers from
/// the host instance (null-safe: a missing instance yields inert bundles).
unsafe fn host_controllers(
    host_instance: *const ARADocumentControllerHostInstance,
) -> (HostAudioAccess, HostModelUpdate, HostArchiving) {
    if host_instance.is_null() {
        return (
            HostAudioAccess {
                host_ref: ptr::null_mut(),
                iface: ptr::null(),
            },
            HostModelUpdate {
                host_ref: ptr::null_mut(),
                iface: ptr::null(),
            },
            HostArchiving {
                host_ref: ptr::null_mut(),
                iface: ptr::null(),
            },
        );
    }
    // SAFETY: non-null host instance provided by the host.
    let hi = unsafe { &*host_instance };
    (
        HostAudioAccess {
            host_ref: hi.audio_access_controller_host_ref,
            iface: hi.audio_access_controller_interface,
        },
        HostModelUpdate {
            host_ref: hi.model_update_controller_host_ref,
            iface: hi.model_update_controller_interface,
        },
        HostArchiving {
            host_ref: hi.archiving_controller_host_ref,
            iface: hi.archiving_controller_interface,
        },
    )
}

/// Render data captured for one playback region when the host assigns it
/// to a renderer (model thread): the region's playback/modification times
/// plus a shared handle to its audio source's analysis. The renderer
/// keeps this so the audio thread never touches the controller.
pub(crate) struct RegionRenderInfo {
    pub(crate) start_in_playback_time: f64,
    pub(crate) duration_in_playback_time: f64,
    pub(crate) start_in_modification_time: f64,
    pub(crate) duration_in_modification_time: f64,
    pub(crate) analysis: Arc<AnalysisState>,
}

impl DocumentController {
    /// Resolve a playback-region ref to its render info (region ->
    /// modification -> source analysis). Called on the model thread.
    pub(crate) fn region_render_info(&self, region_ref: *mut c_void) -> Option<RegionRenderInfo> {
        let region = Handle::from_ref(region_ref).and_then(|h| self.playback_regions.get(h))?;
        let modification = self.audio_modifications.get(region.modification)?;
        let source = self.audio_sources.get(modification.source)?;
        Some(RegionRenderInfo {
            start_in_playback_time: region.start_in_playback_time,
            duration_in_playback_time: region.duration_in_playback_time,
            start_in_modification_time: region.start_in_modification_time,
            duration_in_modification_time: region.duration_in_modification_time,
            analysis: Arc::clone(&source.analysis),
        })
    }

    /// Ensure a region's audio source has been analyzed, spawning analysis
    /// on the task pool if not. Called when a region is assigned to a
    /// renderer, so a plug-in that renders from its analysis (a passthrough
    /// caches the source samples there) has it available even if the host
    /// never issued an explicit content-analysis request. Idempotent: skips
    /// sources already analyzed.
    pub(crate) fn ensure_region_source_analyzed(&self, region_ref: *mut c_void) {
        let job = {
            let region = Handle::from_ref(region_ref).and_then(|h| self.playback_regions.get(h));
            let Some(region) = region else { return };
            let Some(modification) = self.audio_modifications.get(region.modification) else {
                return;
            };
            let Some(source) = self.audio_sources.get(modification.source) else {
                return;
            };
            if source.analysis.analyzed.load(Ordering::Acquire) {
                return;
            }
            AnalysisJob {
                access: self.access,
                model: self.model,
                source_host_ref: source.host_ref,
                #[allow(clippy::cast_sign_loss)]
                channel_count: source.channel_count.max(0) as u32,
                sample_count: source.sample_count,
                sample_rate: source.sample_rate,
                analyze: self.analyze,
                state: Arc::clone(&source.analysis),
            }
        };
        let _ = self.spawner.try_spawn(job);
    }
}

/// Recover the controller from the opaque ref. Sound because ARA model
/// calls are single-threaded and non-reentrant.
pub(crate) unsafe fn controller<'a>(c: ARADocumentControllerRef) -> &'a mut DocumentController {
    unsafe { &mut *c.cast::<DocumentController>() }
}

// --- property readers ------------------------------------------------------

/// Read a `Copy` field only if `struct_size` covers it (ARA versioning).
unsafe fn field<T: Copy>(base: *const c_void, struct_size: usize, offset: usize) -> Option<T> {
    if struct_size >= offset + size_of::<T>() {
        // SAFETY: caller guarantees `base` points at a struct of at
        // least `struct_size` bytes; the offset+size fits within it.
        Some(unsafe { ptr::read_unaligned(base.cast::<u8>().add(offset).cast::<T>()) })
    } else {
        None
    }
}

/// Copy a NUL-terminated host string (valid only for the call) into an
/// owned `String`.
unsafe fn owned_str(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: non-null host-provided C string, valid for this call.
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

unsafe fn struct_size(props: *const c_void) -> usize {
    // `structSize` is the mandatory first field of every ARA property
    // struct. SAFETY: the host always provides at least this field.
    unsafe { ptr::read_unaligned(props.cast::<usize>()) }
}

// --- factory back-link + editing + notifications ---------------------------

unsafe extern "C" fn destroy_document_controller(c: ARADocumentControllerRef) {
    if c.is_null() {
        return;
    }
    // SAFETY: `c` is the `DocumentController` pointer from `create_controller`.
    drop(unsafe { Box::from_raw(c.cast::<DocumentController>()) });
}

unsafe extern "C" fn get_factory(c: ARADocumentControllerRef) -> *const ARAFactory {
    unsafe { controller(c) }.factory
}

unsafe extern "C" fn begin_editing(c: ARADocumentControllerRef) {
    unsafe { controller(c) }.editing = true;
}

unsafe extern "C" fn end_editing(c: ARADocumentControllerRef) {
    unsafe { controller(c) }.editing = false;
}

unsafe extern "C" fn notify_model_updates(_c: ARADocumentControllerRef) {
    // No analysis yet (phase 3), so nothing to report back to the host.
}

unsafe extern "C" fn update_document_properties(_c: ARADocumentControllerRef, _p: *const c_void) {
    // The document name is display-only; phase 2 keeps no document-level
    // state beyond the object arenas.
}

// --- musical contexts ------------------------------------------------------

unsafe extern "C" fn create_musical_context(
    c: ARADocumentControllerRef,
    host_ref: *mut c_void,
    props: *const c_void,
) -> *mut c_void {
    let dc = unsafe { controller(c) };
    let ss = unsafe { struct_size(props) };
    let name = unsafe {
        field::<*const c_char>(props, ss, offset_of!(ARAMusicalContextProperties, name))
            .and_then(|p| owned_str(p))
    };
    let order_index = unsafe {
        field::<i32>(
            props,
            ss,
            offset_of!(ARAMusicalContextProperties, order_index),
        )
    }
    .unwrap_or(0);
    dc.musical_contexts
        .insert(MusicalContext {
            host_ref,
            name,
            order_index,
        })
        .to_ref()
}

unsafe extern "C" fn update_musical_context_properties(
    c: ARADocumentControllerRef,
    r: *mut c_void,
    props: *const c_void,
) {
    let dc = unsafe { controller(c) };
    let Some(handle) = Handle::from_ref(r) else {
        return;
    };
    let ss = unsafe { struct_size(props) };
    let name = unsafe {
        field::<*const c_char>(props, ss, offset_of!(ARAMusicalContextProperties, name))
            .and_then(|p| owned_str(p))
    };
    let order_index = unsafe {
        field::<i32>(
            props,
            ss,
            offset_of!(ARAMusicalContextProperties, order_index),
        )
    };
    if let Some(ctx) = dc.musical_contexts.get_mut(handle) {
        ctx.name = name;
        if let Some(oi) = order_index {
            ctx.order_index = oi;
        }
    }
}

unsafe extern "C" fn destroy_musical_context(c: ARADocumentControllerRef, r: *mut c_void) {
    let dc = unsafe { controller(c) };
    if let Some(handle) = Handle::from_ref(r) {
        dc.musical_contexts.remove(handle);
    }
}

// --- region sequences ------------------------------------------------------

unsafe fn read_region_sequence(props: *const c_void) -> (Option<String>, i32, Option<Handle>) {
    let ss = unsafe { struct_size(props) };
    let name = unsafe {
        field::<*const c_char>(props, ss, offset_of!(ARARegionSequenceProperties, name))
            .and_then(|p| owned_str(p))
    };
    let order_index = unsafe {
        field::<i32>(
            props,
            ss,
            offset_of!(ARARegionSequenceProperties, order_index),
        )
    }
    .unwrap_or(0);
    let musical_context = unsafe {
        field::<*mut c_void>(
            props,
            ss,
            offset_of!(ARARegionSequenceProperties, musical_context_ref),
        )
    }
    .and_then(Handle::from_ref);
    (name, order_index, musical_context)
}

unsafe extern "C" fn create_region_sequence(
    c: ARADocumentControllerRef,
    host_ref: *mut c_void,
    props: *const c_void,
) -> *mut c_void {
    let dc = unsafe { controller(c) };
    let (name, order_index, musical_context) = unsafe { read_region_sequence(props) };
    dc.region_sequences
        .insert(RegionSequence {
            host_ref,
            name,
            order_index,
            musical_context,
        })
        .to_ref()
}

unsafe extern "C" fn update_region_sequence_properties(
    c: ARADocumentControllerRef,
    r: *mut c_void,
    props: *const c_void,
) {
    let dc = unsafe { controller(c) };
    let Some(handle) = Handle::from_ref(r) else {
        return;
    };
    let (name, order_index, musical_context) = unsafe { read_region_sequence(props) };
    if let Some(seq) = dc.region_sequences.get_mut(handle) {
        seq.name = name;
        seq.order_index = order_index;
        seq.musical_context = musical_context;
    }
}

unsafe extern "C" fn destroy_region_sequence(c: ARADocumentControllerRef, r: *mut c_void) {
    let dc = unsafe { controller(c) };
    if let Some(handle) = Handle::from_ref(r) {
        dc.region_sequences.remove(handle);
    }
}

// --- audio sources ---------------------------------------------------------

struct SourceProps {
    name: Option<String>,
    persistent_id: Option<String>,
    sample_count: i64,
    sample_rate: f64,
    channel_count: i32,
}

unsafe fn read_source_props(props: *const c_void) -> SourceProps {
    let ss = unsafe { struct_size(props) };
    let get = |off: usize| -> Option<*const c_char> { unsafe { field(props, ss, off) } };
    SourceProps {
        name: get(offset_of!(ARAAudioSourceProperties, name)).and_then(|p| unsafe { owned_str(p) }),
        persistent_id: get(offset_of!(ARAAudioSourceProperties, persistent_id))
            .and_then(|p| unsafe { owned_str(p) }),
        sample_count: unsafe {
            field::<i64>(
                props,
                ss,
                offset_of!(ARAAudioSourceProperties, sample_count),
            )
        }
        .unwrap_or(0),
        sample_rate: unsafe {
            field::<f64>(props, ss, offset_of!(ARAAudioSourceProperties, sample_rate))
        }
        .unwrap_or(0.0),
        channel_count: unsafe {
            field::<i32>(
                props,
                ss,
                offset_of!(ARAAudioSourceProperties, channel_count),
            )
        }
        .unwrap_or(0),
    }
}

unsafe extern "C" fn create_audio_source(
    c: ARADocumentControllerRef,
    host_ref: *mut c_void,
    props: *const c_void,
) -> *mut c_void {
    let dc = unsafe { controller(c) };
    let p = unsafe { read_source_props(props) };
    dc.audio_sources
        .insert(AudioSource {
            host_ref,
            name: p.name,
            persistent_id: p.persistent_id,
            sample_count: p.sample_count,
            sample_rate: p.sample_rate,
            channel_count: p.channel_count,
            samples_access_enabled: false,
            analysis: Arc::new(AnalysisState::new()),
        })
        .to_ref()
}

unsafe extern "C" fn update_audio_source_properties(
    c: ARADocumentControllerRef,
    r: *mut c_void,
    props: *const c_void,
) {
    let dc = unsafe { controller(c) };
    let Some(handle) = Handle::from_ref(r) else {
        return;
    };
    let p = unsafe { read_source_props(props) };
    if let Some(src) = dc.audio_sources.get_mut(handle) {
        src.name = p.name;
        src.persistent_id = p.persistent_id;
        src.sample_count = p.sample_count;
        src.sample_rate = p.sample_rate;
        src.channel_count = p.channel_count;
    }
}

unsafe extern "C" fn enable_audio_source_samples_access(
    c: ARADocumentControllerRef,
    r: *mut c_void,
    enable: ARABool,
) {
    let dc = unsafe { controller(c) };
    if let Some(handle) = Handle::from_ref(r)
        && let Some(src) = dc.audio_sources.get_mut(handle)
    {
        src.samples_access_enabled = enable != kARAFalse;
    }
}

unsafe extern "C" fn deactivate_audio_source_for_undo_history(
    _c: ARADocumentControllerRef,
    _r: *mut c_void,
    _deactivate: ARABool,
) {
    // Undo-history retention is a plug-in optimization; phase 2 keeps
    // every live object regardless.
}

unsafe extern "C" fn destroy_audio_source(c: ARADocumentControllerRef, r: *mut c_void) {
    let dc = unsafe { controller(c) };
    if let Some(handle) = Handle::from_ref(r) {
        dc.audio_sources.remove(handle);
    }
}

// --- audio-source content analysis -----------------------------------------

unsafe extern "C" fn request_audio_source_content_analysis(
    c: ARADocumentControllerRef,
    r: *mut c_void,
    _content_types_count: usize,
    _content_types: *const i32,
) {
    let dc = unsafe { controller(c) };
    let Some(handle) = Handle::from_ref(r) else {
        return;
    };
    // Snapshot what the worker needs, then drop the source borrow before
    // touching `dc.spawner`.
    let job = {
        let Some(src) = dc.audio_sources.get(handle) else {
            return;
        };
        src.analysis.analyzed.store(false, Ordering::Release);
        AnalysisJob {
            access: dc.access,
            model: dc.model,
            source_host_ref: src.host_ref,
            #[allow(clippy::cast_sign_loss)]
            channel_count: src.channel_count.max(0) as u32,
            sample_count: src.sample_count,
            sample_rate: src.sample_rate,
            analyze: dc.analyze,
            state: Arc::clone(&src.analysis),
        }
    };
    // Run off the model thread on the shared task pool. A full queue
    // drops the request (the host re-requests); analysis is idempotent.
    let _ = dc.spawner.try_spawn(job);
}

fn source_analyzed(dc: &DocumentController, r: *mut c_void) -> bool {
    Handle::from_ref(r)
        .and_then(|h| dc.audio_sources.get(h))
        .is_some_and(|s| s.analysis.analyzed.load(Ordering::Acquire))
}

unsafe extern "C" fn is_audio_source_content_available(
    c: ARADocumentControllerRef,
    r: *mut c_void,
    _content_type: i32,
) -> ARABool {
    if source_analyzed(unsafe { controller(c) }, r) {
        kARATrue
    } else {
        kARAFalse
    }
}

unsafe extern "C" fn is_audio_source_content_analysis_incomplete(
    c: ARADocumentControllerRef,
    r: *mut c_void,
    _content_type: i32,
) -> ARABool {
    // Incomplete = a source we know about that hasn't finished analyzing.
    let dc = unsafe { controller(c) };
    let known = Handle::from_ref(r)
        .and_then(|h| dc.audio_sources.get(h))
        .is_some();
    if known && !source_analyzed(dc, r) {
        kARATrue
    } else {
        kARAFalse
    }
}

unsafe extern "C" fn get_audio_source_content_grade(
    c: ARADocumentControllerRef,
    r: *mut c_void,
    _content_type: i32,
) -> ARAContentGrade {
    if source_analyzed(unsafe { controller(c) }, r) {
        kARAContentGradeDetected
    } else {
        kARAContentGradeInitial
    }
}

// --- audio modifications ---------------------------------------------------

unsafe fn read_modification(
    host_ref: *mut c_void,
    source: Handle,
    props: *const c_void,
) -> AudioModification {
    let ss = unsafe { struct_size(props) };
    AudioModification {
        host_ref,
        source,
        name: unsafe {
            field::<*const c_char>(props, ss, offset_of!(ARAAudioModificationProperties, name))
                .and_then(|p| owned_str(p))
        },
        persistent_id: unsafe {
            field::<*const c_char>(
                props,
                ss,
                offset_of!(ARAAudioModificationProperties, persistent_id),
            )
            .and_then(|p| owned_str(p))
        },
    }
}

unsafe extern "C" fn create_audio_modification(
    c: ARADocumentControllerRef,
    source_ref: *mut c_void,
    host_ref: *mut c_void,
    props: *const c_void,
) -> *mut c_void {
    let dc = unsafe { controller(c) };
    let Some(source) = Handle::from_ref(source_ref) else {
        return ptr::null_mut();
    };
    let modification = unsafe { read_modification(host_ref, source, props) };
    dc.audio_modifications.insert(modification).to_ref()
}

unsafe extern "C" fn clone_audio_modification(
    c: ARADocumentControllerRef,
    r: *mut c_void,
    host_ref: *mut c_void,
    props: *const c_void,
) -> *mut c_void {
    let dc = unsafe { controller(c) };
    // Clone shares the original's parent audio source.
    let Some(source) =
        Handle::from_ref(r).and_then(|h| dc.audio_modifications.get(h).map(|m| m.source))
    else {
        return ptr::null_mut();
    };
    let modification = unsafe { read_modification(host_ref, source, props) };
    dc.audio_modifications.insert(modification).to_ref()
}

unsafe extern "C" fn update_audio_modification_properties(
    c: ARADocumentControllerRef,
    r: *mut c_void,
    props: *const c_void,
) {
    let dc = unsafe { controller(c) };
    let Some(handle) = Handle::from_ref(r) else {
        return;
    };
    let ss = unsafe { struct_size(props) };
    let name = unsafe {
        field::<*const c_char>(props, ss, offset_of!(ARAAudioModificationProperties, name))
            .and_then(|p| owned_str(p))
    };
    let persistent_id = unsafe {
        field::<*const c_char>(
            props,
            ss,
            offset_of!(ARAAudioModificationProperties, persistent_id),
        )
        .and_then(|p| owned_str(p))
    };
    if let Some(m) = dc.audio_modifications.get_mut(handle) {
        m.name = name;
        m.persistent_id = persistent_id;
    }
}

unsafe extern "C" fn deactivate_audio_modification_for_undo_history(
    _c: ARADocumentControllerRef,
    _r: *mut c_void,
    _deactivate: ARABool,
) {
}

unsafe extern "C" fn destroy_audio_modification(c: ARADocumentControllerRef, r: *mut c_void) {
    let dc = unsafe { controller(c) };
    if let Some(handle) = Handle::from_ref(r) {
        dc.audio_modifications.remove(handle);
    }
}

// --- playback regions ------------------------------------------------------

unsafe fn read_playback_region(
    host_ref: *mut c_void,
    modification: Handle,
    props: *const c_void,
) -> PlaybackRegion {
    let ss = unsafe { struct_size(props) };
    let f64_at = |off: usize| unsafe { field::<f64>(props, ss, off) }.unwrap_or(0.0);
    let region_sequence = unsafe {
        field::<*mut c_void>(
            props,
            ss,
            offset_of!(ARAPlaybackRegionProperties, region_sequence_ref),
        )
    }
    .and_then(Handle::from_ref);
    PlaybackRegion {
        host_ref,
        modification,
        region_sequence,
        start_in_playback_time: f64_at(offset_of!(
            ARAPlaybackRegionProperties,
            start_in_playback_time
        )),
        duration_in_playback_time: f64_at(offset_of!(
            ARAPlaybackRegionProperties,
            duration_in_playback_time
        )),
        start_in_modification_time: f64_at(offset_of!(
            ARAPlaybackRegionProperties,
            start_in_modification_time
        )),
        duration_in_modification_time: f64_at(offset_of!(
            ARAPlaybackRegionProperties,
            duration_in_modification_time
        )),
    }
}

unsafe extern "C" fn create_playback_region(
    c: ARADocumentControllerRef,
    modification_ref: *mut c_void,
    host_ref: *mut c_void,
    props: *const c_void,
) -> *mut c_void {
    let dc = unsafe { controller(c) };
    let Some(modification) = Handle::from_ref(modification_ref) else {
        return ptr::null_mut();
    };
    let region = unsafe { read_playback_region(host_ref, modification, props) };
    dc.playback_regions.insert(region).to_ref()
}

unsafe extern "C" fn update_playback_region_properties(
    c: ARADocumentControllerRef,
    r: *mut c_void,
    props: *const c_void,
) {
    let dc = unsafe { controller(c) };
    let Some(handle) = Handle::from_ref(r) else {
        return;
    };
    let modification = dc
        .playback_regions
        .get(handle)
        .map(|region| region.modification);
    let Some(modification) = modification else {
        return;
    };
    let updated = unsafe { read_playback_region(ptr::null_mut(), modification, props) };
    if let Some(region) = dc.playback_regions.get_mut(handle) {
        region.region_sequence = updated.region_sequence;
        region.start_in_playback_time = updated.start_in_playback_time;
        region.duration_in_playback_time = updated.duration_in_playback_time;
        region.start_in_modification_time = updated.start_in_modification_time;
        region.duration_in_modification_time = updated.duration_in_modification_time;
    }
}

unsafe extern "C" fn destroy_playback_region(c: ARADocumentControllerRef, r: *mut c_void) {
    let dc = unsafe { controller(c) };
    if let Some(handle) = Handle::from_ref(r) {
        dc.playback_regions.remove(handle);
    }
}

// --- inert content / archive slots (phases 3-5) ----------------------------

unsafe extern "C" fn ret_i32_2(_c: *mut c_void, _r: *mut c_void) -> i32 {
    kARAFalse
}
unsafe extern "C" fn ret_i32_3(_c: *mut c_void, _r: *mut c_void, _t: i32) -> i32 {
    kARAFalse
}
unsafe extern "C" fn ret_ref_reader(
    _c: *mut c_void,
    _r: *mut c_void,
    _t: i32,
    _range: *const c_void,
) -> *mut c_void {
    ptr::null_mut()
}
unsafe extern "C" fn ret_cptr_3(_c: *mut c_void, _r: *mut c_void, _i: i32) -> *const c_void {
    ptr::null()
}
unsafe extern "C" fn noop2(_c: *mut c_void, _r: *mut c_void) {}

// --- persistence (phase 5) -------------------------------------------------

unsafe extern "C" fn playback_region_head_and_tail_time(
    _c: *mut c_void,
    _r: *mut c_void,
    head: *mut f64,
    tail: *mut f64,
) {
    // No content-based fades yet: report zero head/tail.
    if !head.is_null() {
        unsafe { *head = 0.0 };
    }
    if !tail.is_null() {
        unsafe { *tail = 0.0 };
    }
}

/// Serialize every analyzed source into a host archive payload, keyed by
/// its `persistentID`. Shared by the object- and (deprecated ARA-1)
/// document-level store entry points; the filter is ignored because we
/// only persist analyses, and storing them all is always correct.
fn store_sources(dc: &DocumentController, writer_ref: ARAArchiveWriterHostRef) -> ARABool {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for source in dc.audio_sources.iter() {
        let (Some(id), Some(analysis)) =
            (source.persistent_id.as_deref(), source.analysis.current())
        else {
            continue;
        };
        let mut blob = Vec::new();
        (dc.store)(&*analysis, &mut blob);
        entries.push((id.to_owned(), blob));
    }
    let payload = encode_archive(&entries);
    let writer = ArchiveWriter::new(dc.archiving.host_ref, writer_ref, dc.archiving.iface);
    if writer.write_all(&payload) {
        kARATrue
    } else {
        kARAFalse
    }
}

/// Restore analyses from a host archive, matching each entry to a current
/// source by `persistentID` and republishing it (so the source counts as
/// analyzed without re-running analysis).
fn restore_sources(dc: &mut DocumentController, reader_ref: ARAArchiveReaderHostRef) -> ARABool {
    let reader = ArchiveReader::new(dc.archiving.host_ref, reader_ref, dc.archiving.iface);
    let Some(payload) = reader.read_all() else {
        return kARAFalse;
    };
    let Some(entries) = decode_archive(&payload) else {
        return kARAFalse;
    };
    let restore = dc.restore;
    for (id, blob) in entries {
        if let Some(analysis) = restore(&blob) {
            for source in dc.audio_sources.iter_mut() {
                if source.persistent_id.as_deref() == Some(id.as_str()) {
                    source.analysis.publish(Arc::clone(&analysis));
                }
            }
        }
    }
    kARATrue
}

unsafe extern "C" fn store_objects_to_archive(
    c: ARADocumentControllerRef,
    writer_ref: ARAArchiveWriterHostRef,
    _filter: *const c_void,
) -> ARABool {
    store_sources(unsafe { controller(c) }, writer_ref)
}

unsafe extern "C" fn restore_objects_from_archive(
    c: ARADocumentControllerRef,
    reader_ref: ARAArchiveReaderHostRef,
    _filter: *const c_void,
) -> ARABool {
    restore_sources(unsafe { controller(c) }, reader_ref)
}

/// Deprecated ARA-1 whole-document store. A 2.0 host uses the object API
/// above, but sharing the same payload keeps this correct if a host still
/// takes the document path.
unsafe extern "C" fn store_document_to_archive(
    c: ARADocumentControllerRef,
    writer_ref: ARAArchiveWriterHostRef,
) -> ARABool {
    store_sources(unsafe { controller(c) }, writer_ref)
}

/// The ARA-2 restore is bracketed by `begin`/`endRestoringDocumentFromArchive`
/// around `restoreObjectsFromArchive`; the brackets carry no work of their
/// own, so accept them (returning failure here can abort the host's restore).
unsafe extern "C" fn accept_restore_bracket(
    _c: ARADocumentControllerRef,
    _reader_ref: ARAArchiveReaderHostRef,
) -> ARABool {
    kARATrue
}

/// Archive layout: `[u32 count]` then per entry `[u32 idLen][id]
/// [u32 blobLen][blob]`, all lengths little-endian.
fn encode_archive(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    fn put_len(out: &mut Vec<u8>, n: usize) {
        out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes());
    }
    let mut out = Vec::new();
    put_len(&mut out, entries.len());
    for (id, blob) in entries {
        put_len(&mut out, id.len());
        out.extend_from_slice(id.as_bytes());
        put_len(&mut out, blob.len());
        out.extend_from_slice(blob);
    }
    out
}

fn decode_archive(bytes: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let take_u32 = |pos: &mut usize| -> Option<usize> {
        let end = pos.checked_add(4)?;
        let slice = bytes.get(*pos..end)?;
        *pos = end;
        Some(u32::from_le_bytes(slice.try_into().ok()?) as usize)
    };
    let mut pos = 0usize;
    let count = take_u32(&mut pos)?;
    let mut entries = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let id_len = take_u32(&mut pos)?;
        let id_end = pos.checked_add(id_len)?;
        let id = String::from_utf8(bytes.get(pos..id_end)?.to_vec()).ok()?;
        pos = id_end;
        let blob_len = take_u32(&mut pos)?;
        let blob_end = pos.checked_add(blob_len)?;
        let blob = bytes.get(pos..blob_end)?.to_vec();
        pos = blob_end;
        entries.push((id, blob));
    }
    Some(entries)
}

/// The single, non-generic document-controller vtable. `get_factory`
/// reads the factory pointer off the controller, so no per-`P`
/// monomorphization is needed. `struct_size` covers the ARA-1 mandatory
/// prefix (including archive store/restore) plus the ARA-2 model-graph
/// addenda we implement (region sequences), and stops before the
/// still-inert processing-algorithm addenda.
static VTABLE: ARADocumentControllerInterface = ARADocumentControllerInterface {
    struct_size: offset_of!(
        ARADocumentControllerInterface,
        get_processing_algorithms_count
    ),

    destroy_document_controller: Some(destroy_document_controller),
    get_factory: Some(get_factory),

    begin_editing: Some(begin_editing),
    end_editing: Some(end_editing),
    notify_model_updates: Some(notify_model_updates),

    begin_restoring_document_from_archive: Some(accept_restore_bracket),
    end_restoring_document_from_archive: Some(accept_restore_bracket),
    store_document_to_archive: Some(store_document_to_archive),
    update_document_properties: Some(update_document_properties),

    create_musical_context: Some(create_musical_context),
    update_musical_context_properties: Some(update_musical_context_properties),
    update_musical_context_content: Some(noop_ref_ptr_flags),
    destroy_musical_context: Some(destroy_musical_context),

    create_audio_source: Some(create_audio_source),
    update_audio_source_properties: Some(update_audio_source_properties),
    update_audio_source_content: Some(noop_ref_ptr_flags),
    enable_audio_source_samples_access: Some(enable_audio_source_samples_access),
    deactivate_audio_source_for_undo_history: Some(deactivate_audio_source_for_undo_history),
    destroy_audio_source: Some(destroy_audio_source),

    create_audio_modification: Some(create_audio_modification),
    clone_audio_modification: Some(clone_audio_modification),
    update_audio_modification_properties: Some(update_audio_modification_properties),
    deactivate_audio_modification_for_undo_history: Some(
        deactivate_audio_modification_for_undo_history,
    ),
    destroy_audio_modification: Some(destroy_audio_modification),

    create_playback_region: Some(create_playback_region),
    update_playback_region_properties: Some(update_playback_region_properties),
    destroy_playback_region: Some(destroy_playback_region),

    is_audio_source_content_available: Some(is_audio_source_content_available),
    is_audio_source_content_analysis_incomplete: Some(is_audio_source_content_analysis_incomplete),
    request_audio_source_content_analysis: Some(request_audio_source_content_analysis),
    get_audio_source_content_grade: Some(get_audio_source_content_grade),
    create_audio_source_content_reader: Some(ret_ref_reader),
    is_audio_modification_content_available: Some(ret_i32_3),
    get_audio_modification_content_grade: Some(ret_i32_3),
    create_audio_modification_content_reader: Some(ret_ref_reader),
    is_playback_region_content_available: Some(ret_i32_3),
    get_playback_region_content_grade: Some(ret_i32_3),
    create_playback_region_content_reader: Some(ret_ref_reader),
    get_content_reader_event_count: Some(ret_i32_2),
    get_content_reader_data_for_event: Some(ret_cptr_3),
    destroy_content_reader: Some(noop2),

    create_region_sequence: Some(create_region_sequence),
    update_region_sequence_properties: Some(update_region_sequence_properties),
    destroy_region_sequence: Some(destroy_region_sequence),

    get_playback_region_head_and_tail_time: Some(playback_region_head_and_tail_time),
    restore_objects_from_archive: Some(restore_objects_from_archive),
    store_objects_to_archive: Some(store_objects_to_archive),

    // Still inert (analysis algorithms, licensing, audio-file chunks).
    get_processing_algorithms_count: None,
    get_processing_algorithm_properties: None,
    get_processing_algorithm_for_audio_source: None,
    request_processing_algorithm_for_audio_source: None,
    is_licensed_for_capabilities: None,
    store_audio_source_to_audio_file_chunk: None,
    is_audio_modification_preserving_audio_source_signal: None,
};

unsafe extern "C" fn noop_ref_ptr_flags(
    _c: *mut c_void,
    _r: *mut c_void,
    _p: *const c_void,
    _f: i32,
) {
}

#[cfg(test)]
mod tests {
    use core::slice;
    use std::sync::Mutex;

    use crate::abi::ARAAudioSourceRef;

    use super::*;

    /// Analysis stub for tests: reports one progress update and returns
    /// the source's sample count as the (type-erased) analysis result, so
    /// a test can prove `analyze` ran and read the source.
    fn test_analyze(
        source: &AraAudioSource<'_>,
        progress: &AnalysisProgress,
    ) -> Arc<dyn Any + Send + Sync> {
        progress.update(0.5);
        Arc::new(source.sample_count())
    }

    /// Serialize the `i64` analysis produced by [`test_analyze`].
    fn test_store(analysis: &dyn Any, out: &mut Vec<u8>) {
        if let Some(v) = analysis.downcast_ref::<i64>() {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }

    fn test_restore(bytes: &[u8]) -> Option<Arc<dyn Any + Send + Sync>> {
        let arr: [u8; 8] = bytes.try_into().ok()?;
        Some(Arc::new(i64::from_le_bytes(arr)))
    }

    /// Drive the controller through a scripted model-graph edit exactly
    /// as a host would through the C vtable, and assert the arenas track
    /// creation, parent linkage, update, and destruction. No DAW needed.
    #[test]
    fn model_graph_lifecycle() {
        // Null factory / host instance: this path never calls them.
        let inst = create_controller(
            ptr::null(),
            ptr::null(),
            test_analyze,
            test_store,
            test_restore,
        );
        // SAFETY: `create_controller` returns a valid instance whose
        // `document_controller_ref` is the controller pointer.
        let c = unsafe { (*inst).document_controller_ref };

        unsafe {
            begin_editing(c);

            let source_props = ARAAudioSourceProperties {
                struct_size: size_of::<ARAAudioSourceProperties>(),
                name: c"vocals".as_ptr(),
                persistent_id: c"src-1".as_ptr(),
                sample_count: 96_000,
                sample_rate: 48_000.0,
                channel_count: 2,
                merits_64bit_samples: kARAFalse,
                channel_arrangement_data_type: 0,
                channel_arrangement: ptr::null(),
            };
            let src = create_audio_source(c, ptr::null_mut(), ptr::from_ref(&source_props).cast());
            assert!(!src.is_null());

            let mod_props = ARAAudioModificationProperties {
                struct_size: size_of::<ARAAudioModificationProperties>(),
                name: c"vocals edit".as_ptr(),
                persistent_id: c"mod-1".as_ptr(),
            };
            let modification = create_audio_modification(
                c,
                src,
                ptr::null_mut(),
                ptr::from_ref(&mod_props).cast(),
            );
            assert!(!modification.is_null());

            let region_props = ARAPlaybackRegionProperties {
                struct_size: size_of::<ARAPlaybackRegionProperties>(),
                transformation_flags: 0,
                start_in_modification_time: 0.0,
                duration_in_modification_time: 2.0,
                start_in_playback_time: 4.0,
                duration_in_playback_time: 2.0,
                musical_context_ref: ptr::null_mut(),
                region_sequence_ref: ptr::null_mut(),
                name: ptr::null(),
                color: ptr::null(),
            };
            let region = create_playback_region(
                c,
                modification,
                ptr::null_mut(),
                ptr::from_ref(&region_props).cast(),
            );
            assert!(!region.is_null());

            end_editing(c);

            // Inspect the graph directly (same module, private access).
            let dc = &*c.cast::<DocumentController>();
            assert_eq!(dc.audio_sources.len(), 1);
            assert_eq!(dc.audio_modifications.len(), 1);
            assert_eq!(dc.playback_regions.len(), 1);
            // Parent linkage survived the ref encode/decode round-trip.
            let src_handle = Handle::from_ref(src).unwrap();
            let region_handle = Handle::from_ref(region).unwrap();
            let mod_of_region = dc.playback_regions.get(region_handle).unwrap().modification;
            assert_eq!(
                dc.audio_modifications.get(mod_of_region).unwrap().source,
                src_handle
            );
            assert_eq!(
                dc.audio_sources.get(src_handle).unwrap().name.as_deref(),
                Some("vocals")
            );

            // Update the source in place.
            let renamed = ARAAudioSourceProperties {
                name: c"lead vocals".as_ptr(),
                ..source_props
            };
            update_audio_source_properties(c, src, ptr::from_ref(&renamed).cast());
            let dc = &*c.cast::<DocumentController>();
            assert_eq!(
                dc.audio_sources.get(src_handle).unwrap().name.as_deref(),
                Some("lead vocals")
            );

            // Tear the graph down leaf-first.
            destroy_playback_region(c, region);
            destroy_audio_modification(c, modification);
            destroy_audio_source(c, src);
            let dc = &*c.cast::<DocumentController>();
            assert_eq!(dc.audio_sources.len(), 0);
            assert_eq!(dc.audio_modifications.len(), 0);
            assert_eq!(dc.playback_regions.len(), 0);

            destroy_document_controller(c);
        }
    }

    // Mock host audio-access + model-update controllers for the analysis
    // orchestration test.
    static READER: u8 = 0;
    static PROGRESS_CALLS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

    unsafe extern "C" fn mock_create_reader(
        _h: *mut c_void,
        _s: *mut c_void,
        _use64: ARABool,
    ) -> *mut c_void {
        ptr::from_ref(&READER).cast_mut().cast()
    }
    unsafe extern "C" fn mock_read(
        _h: *mut c_void,
        _r: *mut c_void,
        _pos: i64,
        count: i64,
        buffers: *const *mut c_void,
    ) -> ARABool {
        // Zero-fill both channels; the test only checks that analyze ran.
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        for ch in 0..2usize {
            let buf = unsafe { *buffers.add(ch) }.cast::<f32>();
            for i in 0..count as usize {
                unsafe { *buf.add(i) = 0.0 };
            }
        }
        kARATrue
    }
    unsafe extern "C" fn mock_destroy(_h: *mut c_void, _r: *mut c_void) {}
    unsafe extern "C" fn mock_progress(_h: *mut c_void, _s: *mut c_void, _state: i32, _value: f32) {
        PROGRESS_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    /// Drive create-source -> request-analysis and verify the analysis
    /// runs off-thread on the task pool: the result is published and
    /// progress is reported back to the host. Uses the real global pool.
    #[test]
    fn analysis_runs_on_task_pool() {
        PROGRESS_CALLS.store(0, Ordering::SeqCst);
        let access = crate::abi::ARAAudioAccessControllerInterface {
            struct_size: size_of::<crate::abi::ARAAudioAccessControllerInterface>(),
            create_audio_reader_for_source: Some(mock_create_reader),
            read_audio_samples: Some(mock_read),
            destroy_audio_reader: Some(mock_destroy),
        };
        let model = crate::abi::ARAModelUpdateControllerInterface {
            struct_size: size_of::<crate::abi::ARAModelUpdateControllerInterface>(),
            notify_audio_source_analysis_progress: Some(mock_progress),
            notify_audio_source_content_changed: None,
            notify_audio_modification_content_changed: None,
        };
        let host = ARADocumentControllerHostInstance {
            struct_size: size_of::<ARADocumentControllerHostInstance>(),
            audio_access_controller_host_ref: ptr::null_mut(),
            audio_access_controller_interface: ptr::from_ref(&access),
            archiving_controller_host_ref: ptr::null_mut(),
            archiving_controller_interface: ptr::null(),
            content_access_controller_host_ref: ptr::null_mut(),
            content_access_controller_interface: ptr::null(),
            model_update_controller_host_ref: ptr::null_mut(),
            model_update_controller_interface: ptr::from_ref(&model),
            playback_controller_host_ref: ptr::null_mut(),
            playback_controller_interface: ptr::null(),
        };

        let inst = create_controller(
            ptr::null(),
            ptr::from_ref(&host),
            test_analyze,
            test_store,
            test_restore,
        );
        // SAFETY: valid instance from create_controller.
        let c = unsafe { (*inst).document_controller_ref };

        let src = unsafe {
            begin_editing(c);
            let props = ARAAudioSourceProperties {
                struct_size: size_of::<ARAAudioSourceProperties>(),
                name: c"clip".as_ptr(),
                persistent_id: c"clip-1".as_ptr(),
                sample_count: 12_345,
                sample_rate: 48_000.0,
                channel_count: 2,
                merits_64bit_samples: kARAFalse,
                channel_arrangement_data_type: 0,
                channel_arrangement: ptr::null(),
            };
            let src = create_audio_source(c, ptr::null_mut(), ptr::from_ref(&props).cast());
            end_editing(c);
            // Before analysis: content unavailable, analysis incomplete.
            assert_eq!(is_audio_source_content_available(c, src, 0), kARAFalse);
            assert_eq!(
                is_audio_source_content_analysis_incomplete(c, src, 0),
                kARATrue
            );
            request_audio_source_content_analysis(c, src, 0, ptr::null());
            src
        };

        // Wait for the pool worker (bounded).
        let handle = Handle::from_ref(src).unwrap();
        let mut analyzed = false;
        for _ in 0..400 {
            // SAFETY: controller still alive.
            let dc = unsafe { &*c.cast::<DocumentController>() };
            if dc
                .audio_sources
                .get(handle)
                .is_some_and(|s| s.analysis.analyzed.load(Ordering::Acquire))
            {
                analyzed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(analyzed, "analysis did not complete on the task pool");

        unsafe {
            // Content is now available and analysis complete.
            assert_eq!(is_audio_source_content_available(c, src, 0), kARATrue);
            assert_eq!(
                is_audio_source_content_analysis_incomplete(c, src, 0),
                kARAFalse
            );
            // The published result is the source's sample count.
            let dc = &*c.cast::<DocumentController>();
            let published = dc
                .audio_sources
                .get(handle)
                .unwrap()
                .analysis
                .load()
                .expect("analysis published");
            let value = published.downcast_ref::<i64>().copied();
            assert_eq!(value, Some(12_345));
            // Host saw progress (start + update + complete).
            assert!(PROGRESS_CALLS.load(Ordering::SeqCst) >= 3);
            destroy_document_controller(c);
        }
    }

    // Mock host archive: one shared byte buffer the writer fills and the
    // reader drains, standing in for the DAW's session file.
    static ARCHIVE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    unsafe extern "C" fn arc_write(
        _h: ARAArchivingControllerHostRef,
        _w: ARAArchiveWriterHostRef,
        pos: usize,
        len: usize,
        src: *const u8,
    ) -> ARABool {
        let mut buf = ARCHIVE.lock().unwrap();
        let end = pos + len;
        if buf.len() < end {
            buf.resize(end, 0);
        }
        // SAFETY: the plug-in passes a valid `len`-byte source buffer.
        let bytes = unsafe { slice::from_raw_parts(src, len) };
        buf[pos..end].copy_from_slice(bytes);
        kARATrue
    }

    unsafe extern "C" fn arc_size(
        _h: ARAArchivingControllerHostRef,
        _r: ARAArchiveReaderHostRef,
    ) -> usize {
        ARCHIVE.lock().unwrap().len()
    }

    unsafe extern "C" fn arc_read(
        _h: ARAArchivingControllerHostRef,
        _r: ARAArchiveReaderHostRef,
        pos: usize,
        len: usize,
        out: *mut u8,
    ) -> ARABool {
        let buf = ARCHIVE.lock().unwrap();
        let end = pos + len;
        if buf.len() < end {
            return kARAFalse;
        }
        // SAFETY: the plug-in passes a valid `len`-byte destination buffer.
        let dst = unsafe { slice::from_raw_parts_mut(out, len) };
        dst.copy_from_slice(&buf[pos..end]);
        kARATrue
    }

    /// Build a host instance whose only wired controller is the mock
    /// archiving one; enough to drive store/restore.
    fn archiving_host(
        iface: &ARAArchivingControllerInterface,
    ) -> ARADocumentControllerHostInstance {
        ARADocumentControllerHostInstance {
            struct_size: size_of::<ARADocumentControllerHostInstance>(),
            audio_access_controller_host_ref: ptr::null_mut(),
            audio_access_controller_interface: ptr::null(),
            archiving_controller_host_ref: ptr::null_mut(),
            archiving_controller_interface: ptr::from_ref(iface),
            content_access_controller_host_ref: ptr::null_mut(),
            content_access_controller_interface: ptr::null(),
            model_update_controller_host_ref: ptr::null_mut(),
            model_update_controller_interface: ptr::null(),
            playback_controller_host_ref: ptr::null_mut(),
            playback_controller_interface: ptr::null(),
        }
    }

    /// Create an audio source named by `persistent_id` and return its ref.
    unsafe fn make_source(c: ARADocumentControllerRef, persistent_id: &CStr) -> ARAAudioSourceRef {
        let props = ARAAudioSourceProperties {
            struct_size: size_of::<ARAAudioSourceProperties>(),
            name: c"clip".as_ptr(),
            persistent_id: persistent_id.as_ptr(),
            sample_count: 96_000,
            sample_rate: 48_000.0,
            channel_count: 2,
            merits_64bit_samples: kARAFalse,
            channel_arrangement_data_type: 0,
            channel_arrangement: ptr::null(),
        };
        unsafe {
            begin_editing(c);
            let src = create_audio_source(c, ptr::null_mut(), ptr::from_ref(&props).cast());
            end_editing(c);
            src
        }
    }

    /// Full persistence round-trip: analyze -> store into a mock archive
    /// -> restore into a fresh controller -> the source's analysis matches
    /// without re-analyzing. Matching is by `persistentID`.
    #[test]
    fn analysis_survives_archive_round_trip() {
        ARCHIVE.lock().unwrap().clear();
        let iface = ARAArchivingControllerInterface {
            struct_size: size_of::<ARAArchivingControllerInterface>(),
            get_archive_size: Some(arc_size),
            read_bytes_from_archive: Some(arc_read),
            write_bytes_to_archive: Some(arc_write),
            notify_document_archiving_progress: None,
            notify_document_unarchiving_progress: None,
        };
        let host = archiving_host(&iface);

        // Controller A: create a source, publish an analysis, store it.
        let inst_a = create_controller(
            ptr::null(),
            ptr::from_ref(&host),
            test_analyze,
            test_store,
            test_restore,
        );
        // SAFETY: valid instance from create_controller.
        let ca = unsafe { (*inst_a).document_controller_ref };
        unsafe {
            let src = make_source(ca, c"clip-1");
            let dc = &*ca.cast::<DocumentController>();
            let handle = Handle::from_ref(src).unwrap();
            dc.audio_sources
                .get(handle)
                .unwrap()
                .analysis
                .publish(Arc::new(4242i64));
            assert_eq!(
                store_objects_to_archive(ca, ptr::null_mut(), ptr::null()),
                kARATrue
            );
            destroy_document_controller(ca);
        }
        assert!(
            !ARCHIVE.lock().unwrap().is_empty(),
            "store wrote nothing to the archive"
        );

        // Controller B: fresh controller, same-id source, restore.
        let inst_b = create_controller(
            ptr::null(),
            ptr::from_ref(&host),
            test_analyze,
            test_store,
            test_restore,
        );
        // SAFETY: valid instance from create_controller.
        let cb = unsafe { (*inst_b).document_controller_ref };
        unsafe {
            let src = make_source(cb, c"clip-1");
            assert_eq!(
                restore_objects_from_archive(cb, ptr::null_mut(), ptr::null()),
                kARATrue
            );
            let dc = &*cb.cast::<DocumentController>();
            let handle = Handle::from_ref(src).unwrap();
            let restored = dc
                .audio_sources
                .get(handle)
                .unwrap()
                .analysis
                .current()
                .expect("analysis restored from archive");
            assert_eq!(restored.downcast_ref::<i64>().copied(), Some(4242));
            destroy_document_controller(cb);
        }
    }
}
