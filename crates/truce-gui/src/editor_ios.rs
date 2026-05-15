//! iOS editor host.
//!
//! The host (an `AUv3` `.appex` via the Swift `AudioUnitViewController`)
//! hands us a parent `UIView*` through
//! [`RawWindowHandle::UiKit`](truce_core::editor::RawWindowHandle::UiKit).
//! We attach a plain child `UIView`, rasterize the layout into a
//! tiny-skia `Pixmap` on each `CADisplayLink` tick, wrap the pixel
//! buffer in a `CGImage`, and set `layer.contents = CGImage`. Core
//! Graphics handles the GPU compositing.
//!
//! Touch events: a small `objc2`-allocated `UIView` subclass forwards
//! `touchesBegan/Moved/Ended/Cancelled:` into truce-gui's
//! `InteractionState` via the shared `render_core` machinery.

#![cfg(target_os = "ios")]

use std::any::type_name;
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, Sel};
use objc2::sel;
use objc2_foundation::{NSPoint, NSRect, NSSize};

use truce_core::editor::{Editor, PluginContext, RawWindowHandle};
use truce_gui_types::interaction::{self, InputEvent, InteractionState, MouseButton, ParamEdit};
use truce_gui_types::layout::{GridLayout, Layout, PluginLayout};
use truce_gui_types::theme::Theme;
use truce_params::Params;

use crate::backend_cpu::CpuBackend;
use crate::platform::EditorScale;
use crate::render_core::{build_snapshot_closures, render_widgets};

/// Built-in editor for iOS — API-compatible with the macOS
/// `BuiltinEditor` at the `Editor` trait level so plugin code and
/// host wrappers stay platform-agnostic. Implements:
///
/// - CPU-only rasterization (tiny-skia → `Pixmap`) into a `CGImage`
///   wrapped into the child `UIView`'s layer contents.
/// - `CADisplayLink`-driven repaint pump on the main thread.
/// - Multi-touch → mouse-equivalent `InputEvent` dispatch into the
///   shared `InteractionState` (one `pointer_id` per finger).
pub struct BuiltinEditor<P: Params + 'static> {
    params: Arc<P>,
    layout: Layout,
    theme: Theme,
    backend: Option<CpuBackend>,
    interaction: InteractionState,
    context: Option<PluginContext>,
    /// State the `CADisplayLink` thunk + touch handlers reach
    /// through. Set on `open()`, cleared on `close()`. `RefCell`
    /// (not `Mutex`) because every access path is main-thread only
    /// — `UIKit` callbacks, `AUv3` view-controller lifecycle, and
    /// `Editor::open`/`close` all dispatch on the main thread — so
    /// the right primitive is single-threaded interior mutability.
    /// A `Mutex` here would deadlock on the main thread if a
    /// `tick:` arrives mid-`close()`; `RefCell::try_borrow_mut`
    /// surfaces the same situation as a recoverable miss.
    inner: Arc<RefCell<Option<Inner<P>>>>,
    needs_repaint: Arc<AtomicBool>,
    scale: EditorScale,
}

// SAFETY: All UIKit interactions happen on the main thread, which is
// where AUv3 view controllers live. The raw pointers in `Inner` and
// the `Arc<RefCell<…>>` slot are only touched from `CADisplayLink`
// callbacks (main thread by definition), `touchesBegan/Moved/Ended`
// (main thread), and `open` / `close` (also main thread). The
// `Editor` trait's `Send` requirement is satisfied because no
// concrete value of `BuiltinEditor` is ever sent across threads —
// the inner `Arc<RefCell<…>>` would be `!Send` if it could be
// inspected by the compiler, but its access discipline is enforced
// by UIKit's main-thread contract instead.
unsafe impl<P: Params + 'static> Send for BuiltinEditor<P> {}

struct Inner<P: Params + 'static> {
    /// `UIView*` we created and added to the host parent. Owned by
    /// the parent's view hierarchy after `addSubview:`; we keep a
    /// raw pointer so `close()` can call `removeFromSuperview`.
    child_view: *mut AnyObject,
    /// `CADisplayLink*` we registered with the run loop. Invalidate
    /// on close to stop the repaint pump.
    display_link: *mut AnyObject,
    /// `NSObject*` (runtime-allocated) that owns the `tick:`
    /// selector the display link targets. Pointer-equivalent to a
    /// retain — the run loop holds a strong ref while the link is
    /// active; we release it on close.
    tick_target: *mut AnyObject,
    /// Bounds in logical points, captured at `open` time. The child
    /// view's frame is fixed to whatever the layout reported — no
    /// runtime resize negotiation yet.
    logical_w: u32,
    logical_h: u32,
    /// Last-painted normalized values (per knob region) for
    /// host-driven param-change detection. Mirrors the macOS
    /// editor's `last_painted_values`.
    last_painted_values: Vec<f32>,
    /// Pinned shared state with the rest of the editor — needed so
    /// the `CADisplayLink` callback can flip `needs_repaint`, route
    /// touch events, and reach the same backend / interaction /
    /// snapshot pipeline the macOS path uses.
    params: Arc<P>,
    layout: Layout,
    theme: Theme,
    backend: Option<CpuBackend>,
    interaction: InteractionState,
    context: Option<PluginContext>,
    needs_repaint: Arc<AtomicBool>,
    scale: EditorScale,
}

impl<P: Params + 'static> BuiltinEditor<P> {
    #[must_use]
    pub fn new(params: Arc<P>, layout: PluginLayout) -> Self {
        Self::new_with(params, Layout::Rows(layout))
    }

    #[must_use]
    pub fn new_grid(params: Arc<P>, layout: GridLayout) -> Self {
        Self::new_with(params, Layout::Grid(layout))
    }

    fn new_with(params: Arc<P>, layout: Layout) -> Self {
        Self {
            params,
            layout,
            theme: Theme::dark(),
            backend: None,
            interaction: InteractionState::default(),
            context: None,
            inner: Arc::new(RefCell::new(None)),
            needs_repaint: Arc::new(AtomicBool::new(true)),
            scale: EditorScale::new(crate::platform::main_screen_scale()),
        }
    }
}

impl<P: Params + 'static> Editor for BuiltinEditor<P> {
    fn size(&self) -> (u32, u32) {
        match &self.layout {
            Layout::Rows(p) => (p.width, p.height),
            Layout::Grid(g) => (g.width, g.height),
        }
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        let RawWindowHandle::UiKit(parent_ptr) = parent else {
            log::warn!("iOS BuiltinEditor::open got non-UiKit parent handle");
            return;
        };
        if parent_ptr.is_null() {
            log::warn!("iOS BuiltinEditor::open got null parent pointer");
            return;
        }

        let (lw, lh) = self.size();

        self.context = Some(context.clone());

        // Move per-frame state into Inner. The editor's own copies
        // back the trait's `&mut self` contract; the live render
        // loop reads through Inner (which the runtime UIView
        // subclass owns via an `Arc::into_raw` pointer).
        let inner = Inner {
            child_view: std::ptr::null_mut(),
            display_link: std::ptr::null_mut(),
            tick_target: std::ptr::null_mut(),
            logical_w: lw,
            logical_h: lh,
            last_painted_values: Vec::new(),
            params: Arc::clone(&self.params),
            layout: self.layout.clone(),
            theme: self.theme.clone(),
            backend: None,
            interaction: std::mem::take(&mut self.interaction),
            context: Some(context),
            needs_repaint: Arc::clone(&self.needs_repaint),
            scale: self.scale.clone(),
        };
        let slot = Arc::clone(&self.inner);
        *slot.borrow_mut() = Some(inner);

        // SAFETY: UIKit + CADisplayLink are main-thread-only; AUv3
        // view controllers call open from main thread. The view
        // subclass we register holds one `Arc::into_raw` pin and
        // releases it on close().
        let (view, link) =
            unsafe { install_editor_view::<P>(parent_ptr.cast(), lw, lh, Arc::clone(&slot)) };
        if view.is_null() {
            log::warn!("iOS BuiltinEditor::open: install_editor_view returned null");
            return;
        }
        if let Some(inner_mut) = slot.borrow_mut().as_mut() {
            inner_mut.child_view = view;
            inner_mut.display_link = link;
            inner_mut.tick_target = view; // view is its own tick target
        }
    }

    fn close(&mut self) {
        let Some(inner) = self.inner.borrow_mut().take() else {
            return;
        };
        // SAFETY: invalidate the display link before releasing the
        // view — the link retains its target, and a pending tick
        // firing on a freed view would crash. The view's ivar
        // pointer is released here via `Arc::from_raw`.
        unsafe {
            if !inner.display_link.is_null() {
                let _: () = msg_send![inner.display_link, invalidate];
                let _: () = msg_send![inner.display_link, release];
            }
            if !inner.child_view.is_null() {
                // Reclaim the Arc the view's ivar was holding.
                let cls: &AnyClass = msg_send![inner.child_view, class];
                let base: *const u8 = inner.child_view.cast();
                let ivar_ptr: *const *mut std::ffi::c_void =
                    base.add(ivar_offset(cls, INNER_PTR_IVAR)).cast();
                let leaked = *ivar_ptr as *const RefCell<Option<Inner<P>>>;
                if !leaked.is_null() {
                    let _ = Arc::from_raw(leaked); // dropped here
                }
                let _: () = msg_send![inner.child_view, removeFromSuperview];
            }
        }
        // Move state back into the editor so a subsequent `open()`
        // resumes where we left off (host might reopen the editor
        // after a brief close without re-instantiating the AU).
        self.interaction = inner.interaction;
        self.backend = inner.backend;
        self.context = None;
    }
}

// ---------------------------------------------------------------------------
// UIView subclass + CADisplayLink + touch handling
// ---------------------------------------------------------------------------

/// Ivar slot holding `Arc::into_raw(Arc<RefCell<Option<Inner<P>>>>)`.
/// `ClassBuilder::add_ivar` takes a `&CStr` so the name must be C
/// string at the type level.
const INNER_PTR_IVAR: &std::ffi::CStr = c"_truce_inner_ptr";

// The framework's `NSRunLoopCommonModes` constant. CADisplayLink's
// run-loop-mode match goes by pointer / hash, not string compare —
// a self-constructed NSString with the same content silently wedges
// the timer.
unsafe extern "C" {
    static NSRunLoopCommonModes: *const AnyObject;
}

/// Create a `UIView` subclass that:
/// - holds the editor's `Inner` pinned via an ivar `Arc` pointer,
/// - drives a `CADisplayLink` that calls `tick:` on itself each
///   frame (the view is both the layer host and the tick target,
///   so one `ObjC` class covers rendering + touch),
/// - implements `touchesBegan:withEvent:` /
///   `touchesMoved:withEvent:` / `touchesEnded:withEvent:` /
///   `touchesCancelled:withEvent:` so the user can drag knobs and
///   the param edits route back to the host via `PluginContext`.
unsafe fn install_editor_view<P: Params + 'static>(
    parent: *mut AnyObject,
    logical_w: u32,
    logical_h: u32,
    inner: Arc<RefCell<Option<Inner<P>>>>,
) -> (*mut AnyObject, *mut AnyObject) {
    unsafe {
        let class_name_owned = format!(
            "TruceiOSEditorView_{:x}",
            fnv1a_64(type_name::<Inner<P>>().as_bytes())
        );
        let class_name = std::ffi::CString::new(class_name_owned).expect("ascii class name");
        let uiview = AnyClass::get(c"UIView").expect("UIView missing");

        let cls: &AnyClass = if let Some(existing) = AnyClass::get(class_name.as_c_str()) {
            existing
        } else {
            {
                let mut builder = ClassBuilder::new(class_name.as_c_str(), uiview)
                    .expect("class name unique per type-monomorphization");
                builder.add_ivar::<*mut std::ffi::c_void>(INNER_PTR_IVAR);
                builder.add_method(
                    sel!(tick:),
                    tick_thunk::<P> as unsafe extern "C" fn(_, _, _),
                );
                builder.add_method(
                    sel!(touchesBegan:withEvent:),
                    touches_began::<P> as unsafe extern "C" fn(_, _, _, _),
                );
                builder.add_method(
                    sel!(touchesMoved:withEvent:),
                    touches_moved::<P> as unsafe extern "C" fn(_, _, _, _),
                );
                builder.add_method(
                    sel!(touchesEnded:withEvent:),
                    touches_ended::<P> as unsafe extern "C" fn(_, _, _, _),
                );
                builder.add_method(
                    sel!(touchesCancelled:withEvent:),
                    touches_cancelled::<P> as unsafe extern "C" fn(_, _, _, _),
                );
                builder.register()
            }
        };
        // (Closes the `let cls: &AnyClass = if let … else { … };` above.)

        let frame = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: f64::from(logical_w),
                height: f64::from(logical_h),
            },
        };
        let alloc: *mut AnyObject = msg_send![cls, alloc];
        let view: *mut AnyObject = msg_send![alloc, initWithFrame: frame];
        if view.is_null() {
            return (std::ptr::null_mut(), std::ptr::null_mut());
        }
        // Background so the first frame doesn't flash through. The
        // tiny-skia rasterizer overwrites layer.contents on each
        // tick.
        let color_cls = AnyClass::get(c"UIColor").expect("UIColor missing");
        let bg: *mut AnyObject = msg_send![color_cls, darkGrayColor];
        let _: () = msg_send![view, setBackgroundColor: bg];
        let _: () = msg_send![view, setUserInteractionEnabled: true];
        // Opt in to multi-touch — UIView's default is single-touch
        // (only the first finger generates `touchesBegan:` events).
        // Multi-finger knob drags rely on every finger producing its
        // own `UITouch` in the begin/move/end batches.
        let _: () = msg_send![view, setMultipleTouchEnabled: true];

        // Pin the Arc into the ivar. Released in close() via
        // `Arc::from_raw`.
        let leaked: *const RefCell<Option<Inner<P>>> = Arc::into_raw(inner);
        let base = view.cast::<u8>();
        let ivar_ptr: *mut *mut std::ffi::c_void =
            base.add(ivar_offset(cls, INNER_PTR_IVAR)).cast();
        *ivar_ptr = leaked as *mut std::ffi::c_void;

        let _: () = msg_send![parent, addSubview: view];

        // CADisplayLink: view targets itself.
        let dl_cls = AnyClass::get(c"CADisplayLink").expect("CADisplayLink missing");
        let link: *mut AnyObject =
            msg_send![dl_cls, displayLinkWithTarget: view, selector: sel!(tick:)];
        if link.is_null() {
            return (view, std::ptr::null_mut());
        }
        let _: () = msg_send![link, retain];
        let run_loop_cls = AnyClass::get(c"NSRunLoop").expect("NSRunLoop missing");
        let main: *mut AnyObject = msg_send![run_loop_cls, mainRunLoop];
        // The framework's exported `NSRunLoopCommonModes` constant
        // (NOT a manually-built NSString with the same content) —
        // CADisplayLink's run-loop-mode match goes by pointer / hash,
        // not string compare, so a self-constructed NSString silently
        // wedges the timer.
        let mode: *const AnyObject = NSRunLoopCommonModes;
        let _: () = msg_send![link, addToRunLoop: main, forMode: mode];
        (view, link)
    }
}

/// Borrow the `Arc<RefCell<Option<Inner<P>>>>` pinned in `self_`'s
/// ivar without consuming the pin. The Arc is dropped in `close()`
/// via `Arc::from_raw`; borrowers must use `Arc::clone` + reseat.
unsafe fn borrow_inner_arc<P: Params + 'static>(
    self_: &AnyObject,
) -> Option<Arc<RefCell<Option<Inner<P>>>>> {
    // SAFETY: the ivar was set in `install_editor_view` to a
    // valid `Arc::into_raw` pointer or null. Reading the ivar and
    // reconstructing the Arc is sound as long as we re-leak the
    // original Arc to keep the refcount stable across the borrow.
    unsafe {
        let cls: &AnyClass = msg_send![self_, class];
        let base: *const u8 = std::ptr::from_ref::<AnyObject>(self_).cast();
        let ivar_ptr: *const *mut std::ffi::c_void =
            base.add(ivar_offset(cls, INNER_PTR_IVAR)).cast();
        let leaked = (*ivar_ptr).cast_const().cast::<RefCell<Option<Inner<P>>>>();
        if leaked.is_null() {
            return None;
        }
        let arc = Arc::from_raw(leaked);
        let cloned = Arc::clone(&arc);
        let _ = Arc::into_raw(arc); // re-pin
        Some(cloned)
    }
}

unsafe extern "C" fn tick_thunk<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    _sender: *mut AnyObject,
) {
    unsafe {
        let Some(arc) = borrow_inner_arc::<P>(self_) else {
            return;
        };
        // `try_borrow_mut` (not `borrow_mut`) so a re-entrant
        // `tick:` arriving mid-`close()` skips this tick instead
        // of panicking. All access is main-thread-bound; the only
        // re-entry source is iOS firing a queued display-link tick
        // after we've already started clearing state.
        let Ok(mut guard) = arc.try_borrow_mut() else {
            return;
        };
        let Some(inner) = guard.as_mut() else { return };
        tick(inner);
    }
}

unsafe extern "C" fn touches_began<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe {
        dispatch_touch::<P>(self_, touches, TouchPhase::Began);
    }
}

unsafe extern "C" fn touches_moved<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe {
        dispatch_touch::<P>(self_, touches, TouchPhase::Moved);
    }
}

unsafe extern "C" fn touches_ended<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe {
        dispatch_touch::<P>(self_, touches, TouchPhase::Ended);
    }
}

unsafe extern "C" fn touches_cancelled<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe {
        // Cancellations are surfaced as "up" so the editor can clear
        // any in-progress drag state; the user might tilt the phone
        // mid-twist and we shouldn't strand the param at the
        // last-moved value with no End to commit it.
        dispatch_touch::<P>(self_, touches, TouchPhase::Ended);
    }
}

#[derive(Clone, Copy)]
enum TouchPhase {
    Began,
    Moved,
    Ended,
}

unsafe fn dispatch_touch<P: Params + 'static>(
    self_: &AnyObject,
    touches: *mut AnyObject,
    phase: TouchPhase,
) {
    unsafe {
        let Some(arc) = borrow_inner_arc::<P>(self_) else {
            return;
        };
        // Same `try_borrow_mut` reasoning as `tick_thunk`: a touch
        // delivered mid-`close()` skips rather than panics.
        let Ok(mut guard) = arc.try_borrow_mut() else {
            return;
        };
        let Some(inner) = guard.as_mut() else { return };

        // Multi-touch fan-out: each `UITouch*` in the NSSet is one
        // finger. Use the pointer (stable across begin/move/end for
        // the same finger) as the `pointer_id` so `InteractionState`
        // can track an independent drag per finger.
        let view_ptr: *mut AnyObject = std::ptr::from_ref::<AnyObject>(self_).cast_mut();
        // `NSSet count` is a cheap O(1) read; reserve the exact
        // capacity so the events vec doesn't grow during the loop.
        let touch_count: usize = msg_send![touches, count];
        let enumerator: *mut AnyObject = msg_send![touches, objectEnumerator];
        let mut events: Vec<InputEvent> = Vec::with_capacity(touch_count);
        events.extend(NSEnumerator(enumerator).map(|touch| {
            let pt: NSPoint = msg_send![touch, locationInView: view_ptr];
            // SAFETY: `pt.x`/`pt.y` come from UIKit in logical
            // points, matching the coordinate space `interaction
            // ::dispatch` expects (the layout's logical-point grid).
            #[allow(clippy::cast_possible_truncation)]
            let (x, y) = (pt.x as f32, pt.y as f32);
            let pointer_id = touch as u64;
            match phase {
                TouchPhase::Began => InputEvent::MouseDown {
                    pointer_id,
                    x,
                    y,
                    button: MouseButton::Left,
                },
                TouchPhase::Moved => InputEvent::MouseMove { pointer_id, x, y },
                TouchPhase::Ended => InputEvent::MouseUp {
                    pointer_id,
                    x,
                    y,
                    button: MouseButton::Left,
                },
            }
        }));
        if events.is_empty() {
            return;
        }

        // Run dispatch against the shared snapshot + interaction
        // state. The returned ParamEdits route through the host
        // bridge (PluginContext::set_param) so automation and the
        // audio thread see the change.
        let closures = build_snapshot_closures(&inner.params, inner.context.as_ref());
        let snapshot = closures.as_snapshot();
        let edits =
            interaction::dispatch(&events, &inner.layout, &snapshot, &mut inner.interaction);
        let context = inner.context.clone();
        let params = Arc::clone(&inner.params);
        let needs_repaint = Arc::clone(&inner.needs_repaint);
        drop(guard);
        drop(arc);
        for edit in edits {
            apply_edit(context.as_ref(), &params, &needs_repaint, edit);
        }
    }
}

fn apply_edit<P: Params + 'static>(
    context: Option<&PluginContext>,
    params: &Arc<P>,
    needs_repaint: &Arc<AtomicBool>,
    edit: ParamEdit,
) {
    match edit {
        ParamEdit::Begin { id } => {
            if let Some(ctx) = context {
                ctx.begin_edit(id);
            }
        }
        ParamEdit::Set { id, normalized } => {
            params.set_normalized(id, f64::from(normalized));
            if let Some(ctx) = context {
                ctx.set_param(id, f64::from(normalized));
            }
            needs_repaint.store(true, Ordering::Release);
        }
        ParamEdit::End { id } => {
            if let Some(ctx) = context {
                ctx.end_edit(id);
            }
        }
    }
}

fn tick<P: Params + 'static>(inner: &mut Inner<P>) {
    // Repaint policy: every CADisplayLink fire. The macOS editor
    // short-circuits on `needs_repaint`; until host-driven param-
    // change detection lands here we repaint unconditionally so a
    // host-side automation tick is reflected on the next frame.
    let _ = inner.needs_repaint.swap(false, Ordering::AcqRel);

    let (w, h) = (inner.logical_w, inner.logical_h);
    let scale = inner.scale.get_f32();
    let closures = build_snapshot_closures(&inner.params, inner.context.as_ref());
    let snapshot = closures.as_snapshot();
    let backend = inner.backend.get_or_insert_with(|| {
        CpuBackend::new(w, h, scale).expect("CpuBackend allocation failed (out of memory?)")
    });
    render_widgets(
        &inner.layout,
        &inner.theme,
        &mut inner.interaction,
        &snapshot,
        backend,
    );
    // Rebuild hit-test regions from the current layout so the
    // next touch event can map (x, y) → knob via
    // `interaction::dispatch`. Mirrors the macOS editor's
    // `update_interaction` call site.
    match &inner.layout {
        Layout::Rows(pl) => inner.interaction.build_regions(pl),
        Layout::Grid(gl) => inner.interaction.build_regions_grid(gl),
    }
    // Snapshot the post-render values for next-frame
    // change-detection — currently informational; the iOS pump
    // repaints every CADisplayLink tick regardless.
    inner
        .last_painted_values
        .resize(inner.interaction.knob_regions.len(), 0.0);
    for (slot, r) in inner
        .last_painted_values
        .iter_mut()
        .zip(inner.interaction.knob_regions.iter())
    {
        *slot = r.normalized_value;
    }

    // Blit the freshly-rasterized pixmap into the view's backing
    // layer via a `CGImage`. Core Graphics handles the GPU
    // composite step; we just hand it the pixel buffer.
    unsafe {
        blit_pixmap_to_layer(
            inner.child_view,
            backend.width(),
            backend.height(),
            backend.data(),
        );
    }
}

// ---------------------------------------------------------------------------
// CGImage blit
// ---------------------------------------------------------------------------

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDataProviderCreateWithData(
        info: *mut std::ffi::c_void,
        data: *const u8,
        size: usize,
        release_callback: Option<unsafe extern "C" fn(*mut std::ffi::c_void, *const u8, usize)>,
    ) -> *mut std::ffi::c_void;
    fn CGDataProviderRelease(provider: *mut std::ffi::c_void);
    fn CGColorSpaceCreateDeviceRGB() -> *mut std::ffi::c_void;
    fn CGColorSpaceRelease(cs: *mut std::ffi::c_void);
    fn CGImageCreate(
        width: usize,
        height: usize,
        bits_per_component: usize,
        bits_per_pixel: usize,
        bytes_per_row: usize,
        color_space: *mut std::ffi::c_void,
        bitmap_info: u32,
        provider: *mut std::ffi::c_void,
        decode: *const f32,
        should_interpolate: bool,
        intent: i32,
    ) -> *mut std::ffi::c_void;
    fn CGImageRelease(image: *mut std::ffi::c_void);
}

const K_CG_BITMAP_BYTE_ORDER_32_BIG: u32 = 4 << 12;
// tiny-skia outputs RGBA premultiplied; matches kCGImageAlphaPremultipliedLast.
const K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST: u32 = 1;
const K_CG_RENDERING_INTENT_DEFAULT: i32 = 0;

unsafe fn blit_pixmap_to_layer(view: *mut AnyObject, width: u32, height: u32, rgba: &[u8]) {
    // SAFETY: main-thread Core Graphics + UIKit calls. The pixel
    // buffer outlives the CGImage because the data provider's
    // release callback is None — Core Graphics treats the buffer
    // as borrowed for the image's lifetime, and we release the
    // image at the end of this fn. The CpuBackend reuses its
    // pixmap across frames so the buffer pointer stays stable.
    unsafe {
        let bytes_per_row = (width as usize) * 4;
        let provider =
            CGDataProviderCreateWithData(std::ptr::null_mut(), rgba.as_ptr(), rgba.len(), None);
        if provider.is_null() {
            return;
        }
        let cs = CGColorSpaceCreateDeviceRGB();
        let info = K_CG_BITMAP_BYTE_ORDER_32_BIG | K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST;
        let image = CGImageCreate(
            width as usize,
            height as usize,
            8,
            32,
            bytes_per_row,
            cs,
            info,
            provider,
            std::ptr::null(),
            false,
            K_CG_RENDERING_INTENT_DEFAULT,
        );
        CGDataProviderRelease(provider);
        CGColorSpaceRelease(cs);
        if image.is_null() {
            return;
        }
        let layer: *mut AnyObject = msg_send![view, layer];
        let _: () = msg_send![layer, setContents: image];
        CGImageRelease(image);
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Idiomatic `Iterator` wrapper around an `NSEnumerator*`. The
/// underlying `ObjC` enumerator yields the next element on each
/// `nextObject` call and signals exhaustion with `nil`. Made into
/// a real iterator so caller sites can use `.map().collect()`
/// instead of an open-coded `loop { let obj = next; if nil break }`.
struct NSEnumerator(*mut AnyObject);

impl Iterator for NSEnumerator {
    type Item = *mut AnyObject;
    fn next(&mut self) -> Option<Self::Item> {
        // SAFETY: caller must guarantee `self.0` was obtained from
        // `[set objectEnumerator]` or similar and outlives the
        // iterator. `nextObject` returns nil when exhausted; we
        // never deref the returned pointer here.
        let obj: *mut AnyObject = unsafe { msg_send![self.0, nextObject] };
        (!obj.is_null()).then_some(obj)
    }
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    // Tiny non-crypto hash for class-name uniqueness — std doesn't
    // expose a stable hash without a key, so we hand-roll FNV-1a.
    // Collisions would just keep the previously registered class
    // active for both monomorphizations, which is benign here (the
    // methods are identical instantiations).
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

unsafe fn ivar_offset(cls: &AnyClass, name: &std::ffi::CStr) -> usize {
    // SAFETY: `class_getInstanceVariable` returns NULL if the class
    // doesn't have the ivar; `install_editor_view` always adds it
    // before any instance is allocated, so a null here is a logic
    // bug. `ivar_getOffset` returns the byte offset from the
    // instance start.
    unsafe extern "C" {
        fn class_getInstanceVariable(
            cls: *const AnyClass,
            name: *const std::os::raw::c_char,
        ) -> *mut std::ffi::c_void;
        fn ivar_getOffset(ivar: *mut std::ffi::c_void) -> isize;
    }
    unsafe {
        let ivar = class_getInstanceVariable(std::ptr::from_ref::<AnyClass>(cls), name.as_ptr());
        assert!(!ivar.is_null(), "ivar {name:?} not registered");
        let off = ivar_getOffset(ivar);
        usize::try_from(off).expect("non-negative ivar offset")
    }
}
