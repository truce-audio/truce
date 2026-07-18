//! The ARA playback renderer (phase 4).
//!
//! When the host binds a companion instance with the playback-renderer
//! role, it assigns playback regions to the renderer (on the model
//! thread, while the renderer is inactive). Each assignment captures the
//! region's times plus a shared handle to its source analysis, so the
//! audio thread renders from captured data without touching the
//! controller. [`PlaybackRenderer::render`] reads the latest analysis
//! lock-free and calls the plug-in's `render_regions`.

use core::any::Any;
use core::ffi::c_void;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use truce_core::AudioBuffer;
use truce_core::process::ProcessStatus;

use crate::abi::{
    ARAEditorRendererInterface, ARAEditorRendererRef, ARAEditorViewInterface, ARAEditorViewRef,
    ARAPlaybackRegionRef, ARAPlaybackRendererInterface, ARAPlaybackRendererRef,
    ARAPlugInExtensionInstance, ARAPlugInInstanceRoleFlags, ARARegionSequenceRef, ARASize,
    kARAEditorRendererRole, kARAEditorViewRole, kARAPlaybackRendererRole,
};
use crate::controller::{Published, RegionRenderInfo, controller};
use crate::plugin::{AraPlugin, AraRenderContext, PlaybackRegion};

/// One region assigned to the renderer: its captured render info plus the
/// most recent analysis pulled off the lock-free slot.
struct AssignedRegion {
    region_ref: ARAPlaybackRegionRef,
    info: RegionRenderInfo,
    cached: Option<Arc<dyn Any + Send + Sync>>,
}

/// The playback renderer for one bound instance.
pub struct PlaybackRenderer {
    /// The document controller this instance is bound to (used only on
    /// the model thread, when assigning/removing regions).
    controller: *mut c_void,
    /// This instance's assigned ARA roles, surfaced to `render_regions`
    /// via [`AraRenderContext::roles`] so a plug-in can tell playback from
    /// edit-preview.
    roles: ARAPlugInInstanceRoleFlags,
    /// The document's shared params slot (the editor-view instance's
    /// `Arc<P::Params>`). Read lock-free each block so this renderer uses
    /// the params the user actually edits, not its own instance's copy.
    /// Empty until the editor-view instance binds; then the renderer falls
    /// back to the shell's params.
    shared_params: Arc<ArcSwapOption<Published>>,
    regions: Vec<AssignedRegion>,
}

impl PlaybackRenderer {
    fn new(
        controller_ref: *mut c_void,
        roles: ARAPlugInInstanceRoleFlags,
        shared_params: Arc<ArcSwapOption<Published>>,
    ) -> Self {
        Self {
            controller: controller_ref,
            roles,
            shared_params,
            regions: Vec::new(),
        }
    }

    /// Assign a region (model thread). Captures its render info from the
    /// controller so the audio thread never dereferences the controller.
    fn add_region(&mut self, region_ref: ARAPlaybackRegionRef) {
        if self.controller.is_null() {
            return;
        }
        // SAFETY: `controller` is the live document controller; region
        // assignment happens on the model thread, single-threaded.
        let dc = unsafe { controller(self.controller) };
        let info = dc.region_render_info(region_ref);
        // Kick off analysis of the region's source so a plug-in that renders
        // from its analysis has the source data available even without an
        // explicit host content-analysis request.
        dc.ensure_region_source_analyzed(region_ref);
        if let Some(info) = info {
            self.regions.push(AssignedRegion {
                region_ref,
                info,
                cached: None,
            });
        }
    }

    fn remove_region(&mut self, region_ref: ARAPlaybackRegionRef) {
        self.regions.retain(|r| r.region_ref != region_ref);
    }

    /// Render the assigned regions into `buffer` on the audio thread.
    /// Reads each region's latest analysis lock-free and calls
    /// [`AraPlugin::render_regions`].
    ///
    /// Allocates a per-call region list; making the render path fully
    /// alloc-free (a caller-provided reusable region buffer) is a
    /// follow-up. `A` must be the bound plug-in's `AraPlugin` type.
    pub fn render<A: AraPlugin>(
        &mut self,
        state: &mut A::DspState,
        params: &A::Params,
        buffer: &mut AudioBuffer<'_, A::Sample>,
        context: &AraRenderContext<'_>,
    ) -> ProcessStatus {
        let regions = self.collect_regions::<A::Analysis>();
        // Inject this instance's roles - the shell can't know them, but the
        // renderer does. Reuse the shell's transport / sample rate / block.
        let context = AraRenderContext {
            transport: context.transport,
            sample_rate: context.sample_rate,
            block_size: context.block_size,
            roles: self.roles,
        };
        // Prefer the document's shared params (the editor-view instance's,
        // the ones the user edits) over this instance's own copy, so gain
        // and other edits from the visible editor reach playback. Falls back
        // to the shell's params when no editor-view instance has bound.
        let shared = self.shared_params.load_full();
        let params = shared
            .as_ref()
            .and_then(|p| p.0.downcast_ref::<A::Params>())
            .unwrap_or(params);
        A::render_regions(state, params, &regions, buffer, &context)
    }

    /// Build the region list for the plug-in, refreshing each region's
    /// analysis from the lock-free slot. Split from [`Self::render`] so it
    /// can be exercised without a `Params` value (which is sealed).
    fn collect_regions<Analysis: Any + Send + Sync>(&mut self) -> Vec<PlaybackRegion<Analysis>> {
        let mut regions = Vec::with_capacity(self.regions.len());
        for assigned in &mut self.regions {
            if let Some(latest) = assigned.info.analysis.load() {
                assigned.cached = Some(latest);
            }
            let analysis = assigned
                .cached
                .clone()
                .and_then(|a| Arc::downcast::<Analysis>(a).ok());
            regions.push(PlaybackRegion::new(
                analysis,
                assigned.info.start_in_playback_time,
                assigned.info.duration_in_playback_time,
                assigned.info.start_in_modification_time,
                assigned.info.duration_in_modification_time,
            ));
        }
        regions
    }
}

/// Boxed binding handed back from `bind_to_document_controller`. `instance`
/// is at offset 0 so the returned instance pointer and the binding
/// pointer coincide.
#[repr(C)]
pub struct PlaybackBinding {
    instance: ARAPlugInExtensionInstance,
    renderer: PlaybackRenderer,
}

impl PlaybackBinding {
    /// The renderer, for the audio thread to drive via
    /// [`PlaybackRenderer::render`].
    pub fn renderer_mut(&mut self) -> &mut PlaybackRenderer {
        &mut self.renderer
    }
}

/// Create a plug-in extension binding for `controller_ref`. The playback
/// renderer is exposed only when the host assigned the playback role.
/// `instance_params` is this instance's type-erased `Arc<P::Params>`: an
/// editor-view instance publishes it as the document's shared params (so
/// the renderers read what the visible editor edits), and every renderer
/// captures a handle to that shared slot. The returned instance is valid
/// until [`destroy_playback_binding`]. Returns the binding pointer (== the
/// instance pointer) so the caller can both hand the instance to the host
/// and later drive the renderer.
///
/// # Safety
/// `controller_ref` must be a live `DocumentController` pointer from this
/// crate's factory (the host's ARA controller ref for our document), or
/// null.
#[must_use]
pub unsafe fn create_playback_binding(
    controller_ref: *mut c_void,
    assigned_roles: ARAPlugInInstanceRoleFlags,
    instance_params: Arc<dyn Any + Send + Sync>,
) -> *mut PlaybackBinding {
    // Recover the document controller (its ref is our controller pointer)
    // to reach the per-document shared-params slot. A null ref (e.g. a
    // test) leaves this instance with a private, empty slot.
    let shared_params = if controller_ref.is_null() {
        Arc::new(ArcSwapOption::empty())
    } else {
        // SAFETY: caller guarantees `controller_ref` is our live
        // `DocumentController`, shared across the document's instances;
        // bind is single-threaded.
        let dc = unsafe { controller(controller_ref) };
        if (assigned_roles & kARAEditorViewRole) != 0 {
            dc.publish_shared_params(instance_params);
        }
        dc.shared_params_handle()
    };
    let renders = (assigned_roles & kARAPlaybackRendererRole) != 0;
    let mut boxed = Box::new(PlaybackBinding {
        instance: ARAPlugInExtensionInstance {
            struct_size: size_of::<ARAPlugInExtensionInstance>(),
            plug_in_extension_ref: ptr::null_mut(),
            plug_in_extension_interface: ptr::null(),
            playback_renderer_ref: ptr::null_mut(),
            playback_renderer_interface: ptr::null(),
            editor_renderer_ref: ptr::null_mut(),
            editor_renderer_interface: ptr::null(),
            editor_view_ref: ptr::null_mut(),
            editor_view_interface: ptr::null(),
        },
        renderer: PlaybackRenderer::new(controller_ref, assigned_roles, shared_params),
    });
    if renders {
        boxed.instance.playback_renderer_ref = ptr::from_mut(&mut boxed.renderer).cast::<c_void>();
        boxed.instance.playback_renderer_interface =
            ptr::from_ref(&PLAYBACK_VTABLE).cast::<c_void>();
    }
    // Editor renderer (edit preview): reuse the same renderer - it previews
    // the assigned regions exactly as playback does. The host binds this on
    // a separate instance from the playback renderer.
    if (assigned_roles & kARAEditorRendererRole) != 0 {
        boxed.instance.editor_renderer_ref = ptr::from_mut(&mut boxed.renderer).cast::<c_void>();
        boxed.instance.editor_renderer_interface =
            ptr::from_ref(&EDITOR_RENDERER_VTABLE).cast::<c_void>();
    }
    // Editor view: no ARA-aware UI (a passthrough has no selection model),
    // but the interface must be non-null so a host opening the editor
    // doesn't call through a null vtable and crash.
    if (assigned_roles & kARAEditorViewRole) != 0 {
        boxed.instance.editor_view_ref = ptr::from_mut(&mut boxed.renderer).cast::<c_void>();
        boxed.instance.editor_view_interface = ptr::from_ref(&EDITOR_VIEW_VTABLE).cast::<c_void>();
    }
    Box::into_raw(boxed)
}

/// The instance pointer to hand the host (== the binding pointer).
///
/// # Safety
/// `binding` must be a live pointer from [`create_playback_binding`].
#[must_use]
pub unsafe fn binding_instance(binding: *mut PlaybackBinding) -> *const ARAPlugInExtensionInstance {
    // `instance` is at offset 0.
    binding.cast::<ARAPlugInExtensionInstance>()
}

/// The binding's renderer, or null when the instance has no assigned
/// regions (nothing to render). The format wrapper routes `process` ->
/// `PluginRuntime::ara_render` with this pointer.
///
/// Route on region assignment, not on role: an editor-renderer instance
/// the host also uses as its audio output (Reaper binds the take-FX
/// instance this way) gets no regions, so it stays a passthrough instead
/// of rendering silence over the host's own audio, while an editor
/// renderer the host DOES assign regions to (Cubase's edit-preview
/// instance) still renders.
///
/// # Safety
/// `binding` must be a live pointer from [`create_playback_binding`].
#[must_use]
pub unsafe fn binding_renderer(binding: *mut PlaybackBinding) -> *mut PlaybackRenderer {
    if binding.is_null() {
        return ptr::null_mut();
    }
    let instance = unsafe { &(*binding).instance };
    // Playback and editor roles both alias the one `PlaybackRenderer`.
    let renderer = if instance.playback_renderer_ref.is_null() {
        instance.editor_renderer_ref
    } else {
        instance.playback_renderer_ref
    }
    .cast::<PlaybackRenderer>();
    if renderer.is_null() {
        return ptr::null_mut();
    }
    // Region assignment happens on the model thread while the renderer is
    // inactive, so reading the list here on the audio thread never races.
    if unsafe { (*renderer).regions.is_empty() } {
        return ptr::null_mut();
    }
    renderer
}

/// Free a binding from [`create_playback_binding`].
///
/// # Safety
/// `binding` must be a live pointer from [`create_playback_binding`], not
/// used afterward.
pub unsafe fn destroy_playback_binding(binding: *mut PlaybackBinding) {
    if binding.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(binding) });
}

/// Owns one plug-in instance's ARA playback-renderer binding, freeing it
/// when the instance is dropped. Every format wrapper (CLAP / VST3 / AU)
/// stores the binding here at bind time and reads the renderer on each
/// process block, so the store/free/route logic lives once.
pub struct AraBindingCell(AtomicPtr<PlaybackBinding>);

impl AraBindingCell {
    #[must_use]
    pub fn new() -> Self {
        Self(AtomicPtr::new(ptr::null_mut()))
    }

    /// Store a freshly-created binding, freeing any prior one (ARA binds an
    /// instance at most once, so there normally is none).
    pub fn store(&self, binding: *mut PlaybackBinding) {
        let old = self.0.swap(binding, Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: `old` was a live binding we owned; nothing references
            // it after the swap.
            unsafe { destroy_playback_binding(old) };
        }
    }

    /// The bound instance's playback renderer as an opaque pointer, or null
    /// when it isn't bound as a playback renderer (so `process` runs
    /// normally). The audio thread reads this to route into `ara_render`.
    #[must_use]
    pub fn renderer(&self) -> *mut c_void {
        let binding = self.0.load(Ordering::Acquire);
        if binding.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: `binding` is the live binding we stored.
        unsafe { binding_renderer(binding) }.cast::<c_void>()
    }
}

impl Default for AraBindingCell {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AraBindingCell {
    fn drop(&mut self) {
        let binding = self.0.load(Ordering::Acquire);
        if !binding.is_null() {
            // SAFETY: sole owner; the instance is being torn down.
            unsafe { destroy_playback_binding(binding) };
        }
    }
}

static PLAYBACK_VTABLE: ARAPlaybackRendererInterface = ARAPlaybackRendererInterface {
    struct_size: size_of::<ARAPlaybackRendererInterface>(),
    add_playback_region: Some(add_playback_region),
    remove_playback_region: Some(remove_playback_region),
};

unsafe extern "C" fn add_playback_region(
    renderer: ARAPlaybackRendererRef,
    region: ARAPlaybackRegionRef,
) {
    if renderer.is_null() {
        return;
    }
    // SAFETY: `renderer` is the `PlaybackRenderer` pointer we installed as
    // `playback_renderer_ref`; region assignment is on the model thread.
    unsafe { &mut *renderer.cast::<PlaybackRenderer>() }.add_region(region);
}

unsafe extern "C" fn remove_playback_region(
    renderer: ARAPlaybackRendererRef,
    region: ARAPlaybackRegionRef,
) {
    if renderer.is_null() {
        return;
    }
    // SAFETY: as for `add_playback_region`.
    unsafe { &mut *renderer.cast::<PlaybackRenderer>() }.remove_region(region);
}

// The editor renderer previews the edited regions; it reuses the one
// `PlaybackRenderer` (its ref aliases the playback ref), so region
// assignment and the audio-thread render path are shared with playback.
static EDITOR_RENDERER_VTABLE: ARAEditorRendererInterface = ARAEditorRendererInterface {
    struct_size: size_of::<ARAEditorRendererInterface>(),
    add_playback_region: Some(editor_add_playback_region),
    remove_playback_region: Some(editor_remove_playback_region),
    // Region sequences are a host-side shortcut for "all regions on this
    // sequence"; this renderer tracks individual regions (the host uses one
    // scheme or the other per instance), so ignore them.
    add_region_sequence: Some(editor_noop_region_sequence),
    remove_region_sequence: Some(editor_noop_region_sequence),
};

unsafe extern "C" fn editor_add_playback_region(
    renderer: ARAEditorRendererRef,
    region: ARAPlaybackRegionRef,
) {
    if renderer.is_null() {
        return;
    }
    // SAFETY: `renderer` is the shared `PlaybackRenderer` installed as
    // `editor_renderer_ref`; the edit preview renders the same regions.
    unsafe { &mut *renderer.cast::<PlaybackRenderer>() }.add_region(region);
}

unsafe extern "C" fn editor_remove_playback_region(
    renderer: ARAEditorRendererRef,
    region: ARAPlaybackRegionRef,
) {
    if renderer.is_null() {
        return;
    }
    // SAFETY: as for `editor_add_playback_region`.
    unsafe { &mut *renderer.cast::<PlaybackRenderer>() }.remove_region(region);
}

unsafe extern "C" fn editor_noop_region_sequence(
    _renderer: ARAEditorRendererRef,
    _sequence: ARARegionSequenceRef,
) {
}

// A passthrough has no ARA-aware UI, so the editor view ignores host
// selection / hide notifications - the SDK permits empty implementations.
// The interface just has to be non-null so the host doesn't call a null
// vtable when it opens the editor.
static EDITOR_VIEW_VTABLE: ARAEditorViewInterface = ARAEditorViewInterface {
    struct_size: size_of::<ARAEditorViewInterface>(),
    notify_selection: Some(editor_view_noop_selection),
    notify_hide_region_sequences: Some(editor_view_noop_hide),
};

unsafe extern "C" fn editor_view_noop_selection(
    _view: ARAEditorViewRef,
    _selection: *const c_void,
) {
}

unsafe extern "C" fn editor_view_noop_hide(
    _view: ARAEditorViewRef,
    _count: ARASize,
    _sequences: *const ARARegionSequenceRef,
) {
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{
        ARAAudioAccessControllerInterface, ARAAudioModificationProperties,
        ARAAudioSourceProperties, ARABool, ARADocumentControllerHostInstance,
        ARAPlaybackRegionProperties, kARAFalse, kARATrue,
    };
    use crate::controller::create_controller;

    static READER: u8 = 0;

    unsafe extern "C" fn create_reader(
        _h: *mut c_void,
        _s: *mut c_void,
        _b: ARABool,
    ) -> *mut c_void {
        ptr::from_ref(&READER).cast_mut().cast()
    }
    unsafe extern "C" fn read_samples(
        _h: *mut c_void,
        _r: *mut c_void,
        _pos: i64,
        count: i64,
        buffers: *const *mut c_void,
    ) -> ARABool {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        for ch in 0..2usize {
            let buf = unsafe { *buffers.add(ch) }.cast::<f32>();
            for i in 0..count as usize {
                unsafe { *buf.add(i) = 0.0 };
            }
        }
        kARATrue
    }
    unsafe extern "C" fn destroy_reader(_h: *mut c_void, _r: *mut c_void) {}

    /// Analysis stub: the source's sample count, as `i64`.
    fn analyze_sample_count(
        source: &crate::audio_source::AraAudioSource<'_>,
        _progress: &crate::audio_source::AnalysisProgress,
    ) -> Arc<dyn Any + Send + Sync> {
        Arc::new(source.sample_count())
    }

    fn nop_store(_analysis: &dyn Any, _out: &mut Vec<u8>) {}

    fn nop_restore(_bytes: &[u8]) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }

    /// Drive create-source -> analyze -> create-region -> bind-renderer ->
    /// add-region through the C ABI exactly as a host would, then assert
    /// the renderer captured the region and pulled its source analysis.
    #[test]
    #[allow(clippy::float_cmp)]
    fn renderer_captures_regions_and_analysis() {
        let access = ARAAudioAccessControllerInterface {
            struct_size: size_of::<ARAAudioAccessControllerInterface>(),
            create_audio_reader_for_source: Some(create_reader),
            read_audio_samples: Some(read_samples),
            destroy_audio_reader: Some(destroy_reader),
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
            model_update_controller_interface: ptr::null(),
            playback_controller_host_ref: ptr::null_mut(),
            playback_controller_interface: ptr::null(),
        };

        let inst = create_controller(
            ptr::null(),
            ptr::from_ref(&host),
            analyze_sample_count,
            nop_store,
            nop_restore,
        );
        // SAFETY: valid instance; drive it through its own vtable.
        let (region, src) = unsafe {
            let c = (*inst).document_controller_ref;
            let vt = &*(*inst).document_controller_interface;
            (vt.begin_editing.unwrap())(c);

            let src_props = ARAAudioSourceProperties {
                struct_size: size_of::<ARAAudioSourceProperties>(),
                name: c"clip".as_ptr(),
                persistent_id: c"clip".as_ptr(),
                sample_count: 777,
                sample_rate: 48_000.0,
                channel_count: 2,
                merits_64bit_samples: kARAFalse,
                channel_arrangement_data_type: 0,
                channel_arrangement: ptr::null(),
            };
            let src = (vt.create_audio_source.unwrap())(
                c,
                ptr::null_mut(),
                ptr::from_ref(&src_props).cast(),
            );

            let mod_props = ARAAudioModificationProperties {
                struct_size: size_of::<ARAAudioModificationProperties>(),
                name: ptr::null(),
                persistent_id: c"mod".as_ptr(),
            };
            let modification = (vt.create_audio_modification.unwrap())(
                c,
                src,
                ptr::null_mut(),
                ptr::from_ref(&mod_props).cast(),
            );

            let region_props = ARAPlaybackRegionProperties {
                struct_size: size_of::<ARAPlaybackRegionProperties>(),
                transformation_flags: 0,
                start_in_modification_time: 1.0,
                duration_in_modification_time: 2.0,
                start_in_playback_time: 4.0,
                duration_in_playback_time: 2.0,
                musical_context_ref: ptr::null_mut(),
                region_sequence_ref: ptr::null_mut(),
                name: ptr::null(),
                color: ptr::null(),
            };
            let region = (vt.create_playback_region.unwrap())(
                c,
                modification,
                ptr::null_mut(),
                ptr::from_ref(&region_props).cast(),
            );
            (vt.end_editing.unwrap())(c);

            (vt.request_audio_source_content_analysis.unwrap())(c, src, 0, ptr::null());
            // Wait for the pool worker (bounded).
            for _ in 0..400 {
                if (vt.is_audio_source_content_analysis_incomplete.unwrap())(c, src, 0) == kARAFalse
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            (region, src)
        };
        let _ = src;

        // Bind a playback renderer and assign the region.
        let c = unsafe { (*inst).document_controller_ref };
        let binding = unsafe {
            create_playback_binding(
                c,
                kARAPlaybackRendererRole,
                Arc::new(()) as Arc<dyn Any + Send + Sync>,
            )
        };
        unsafe {
            let ext = &*binding_instance(binding);
            assert!(!ext.playback_renderer_interface.is_null());
            let rvt = &*ext
                .playback_renderer_interface
                .cast::<ARAPlaybackRendererInterface>();
            (rvt.add_playback_region.unwrap())(ext.playback_renderer_ref, region);

            // The renderer captured the region and pulled its analysis.
            let regions = (*binding).renderer_mut().collect_regions::<i64>();
            assert_eq!(regions.len(), 1);
            assert_eq!(regions[0].analysis(), Some(&777_i64));
            assert_eq!(regions[0].start_in_playback_time, 4.0);
            assert_eq!(regions[0].duration_in_modification_time, 2.0);

            // Removing it empties the renderer.
            (rvt.remove_playback_region.unwrap())(ext.playback_renderer_ref, region);
            assert_eq!((*binding).renderer_mut().collect_regions::<i64>().len(), 0);

            destroy_playback_binding(binding);
            let vt = &*(*inst).document_controller_interface;
            (vt.destroy_document_controller.unwrap())((*inst).document_controller_ref);
        }
    }

    /// The editor-view instance publishes its params as the document's
    /// shared set, and a separately-bound playback-renderer instance reads
    /// them - the mechanism that lets a knob on the visible editor reach the
    /// instances Cubase actually renders with. `String` stands in for
    /// `P::Params` (the slot is type-erased either way).
    #[test]
    fn editor_view_publishes_shared_params_to_renderers() {
        let inst = create_controller(
            ptr::null(),
            ptr::null(),
            analyze_sample_count,
            nop_store,
            nop_restore,
        );
        // SAFETY: valid instance from create_controller.
        let c = unsafe { (*inst).document_controller_ref };

        let view_params: Arc<dyn Any + Send + Sync> = Arc::new(String::from("view-params"));
        // SAFETY: `c` is our live document controller.
        let view =
            unsafe { create_playback_binding(c, kARAEditorViewRole, Arc::clone(&view_params)) };
        let play = unsafe {
            create_playback_binding(
                c,
                kARAPlaybackRendererRole,
                Arc::new(String::from("render-params")) as Arc<dyn Any + Send + Sync>,
            )
        };

        // SAFETY: both bindings are live; read the renderer's shared slot.
        unsafe {
            let shared = (*play)
                .renderer_mut()
                .shared_params
                .load_full()
                .expect("editor-view instance published shared params");
            assert_eq!(
                shared.0.downcast_ref::<String>().map(String::as_str),
                Some("view-params"),
            );

            destroy_playback_binding(view);
            destroy_playback_binding(play);
            let vt = &*(*inst).document_controller_interface;
            (vt.destroy_document_controller.unwrap())(c);
        }
    }
}
