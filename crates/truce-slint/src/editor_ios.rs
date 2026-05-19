//! Slint editor on iOS - `MinimalSoftwareWindow` driving a tight CPU
//! render pipeline, blitted to a `UIView`'s layer via `CGImage`.
//!
//! Slint's software renderer outputs `PremultipliedRgbaColor` pixels
//! into a buffer we own. We un-premultiply (same path the desktop
//! editor uses for screenshot baselines) and hand the bytes to Core
//! Graphics, which wraps them in a `CGImage` and sets it as the
//! `UIView`'s `layer.contents`. Touch events translate into
//! `slint::WindowEvent::PointerPressed/Moved/Released`.

#![cfg(target_os = "ios")]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, Sel};
use objc2::sel;
use objc2_foundation::{NSPoint, NSRect, NSSize};

use slint::LogicalPosition;
use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor};
use slint::platform::{PointerEventButton, WindowAdapter, WindowEvent};

use truce_core::editor::{Editor, PluginContext, RawWindowHandle};
use truce_gui::ios::{TouchPhase, fnv1a_64, ivar_offset};
use truce_params::Params;

use crate::platform::{create_slint_window, ensure_platform, render_to_rgba};

pub type SyncFn<P> = Box<dyn Fn(&PluginContext<P>)>;

pub type SetupFn<P> = Arc<dyn Fn(PluginContext<P>) -> SyncFn<P> + Send + Sync>;

pub struct SlintEditor<P: Params + ?Sized> {
    params: Arc<P>,
    size: (u32, u32),
    setup: SetupFn<P>,
    inner: Arc<Mutex<Option<Inner<P>>>>,
}

// SAFETY: Same shape as the desktop SlintEditor's unsafe Send impl.
// UIKit + CADisplayLink + Slint are main-thread; the AUv3 host calls
// Editor methods from the main thread; we never send the editor
// across threads in practice.
unsafe impl<P: Params + ?Sized> Send for SlintEditor<P> {}

struct Inner<P: Params + ?Sized> {
    child_view: *mut AnyObject,
    display_link: *mut AnyObject,
    logical_w: u32,
    logical_h: u32,
    scale: f32,
    slint_window: Rc<MinimalSoftwareWindow>,
    /// Renderer pixel buffer - `PremultipliedRgbaColor`-typed so
    /// Slint can write straight into it without a per-frame cast.
    px_buf: Vec<PremultipliedRgbaColor>,
    /// Un-premultiplied RGBA8 bytes ready for `CGImage`.
    rgba_buf: Vec<u8>,
    sync: SyncFn<P>,
    context: PluginContext<P>,
    params: Arc<P>,
    last_pointer: LogicalPosition,
    /// Pending touch handler → tick communication. Touch handlers
    /// run on the main thread (same as tick) but the slint window
    /// `dispatch_event` is cleanest if we batch and apply at the
    /// start of each render frame.
    pending_events: RefCell<Vec<WindowEvent>>,
}

impl<P: Params + 'static> SlintEditor<P> {
    pub fn new(
        params: Arc<P>,
        size: (u32, u32),
        setup: impl Fn(PluginContext<P>) -> SyncFn<P> + Send + Sync + 'static,
    ) -> Self {
        Self {
            params,
            size,
            setup: Arc::new(setup),
            inner: Arc::new(Mutex::new(None)),
        }
    }
}

impl<P: Params + 'static> Editor for SlintEditor<P> {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        let RawWindowHandle::UiKit(parent_ptr) = parent else {
            log::warn!("SlintEditor (iOS) got non-UiKit parent handle");
            return;
        };
        if parent_ptr.is_null() {
            return;
        }
        let (lw, lh) = self.size;
        // `query_backing_scale(parent)` returns the parent's
        // `contentScaleFactor`, which at `gui_open` time is still
        // the default 1.0 (the container UIView hasn't been
        // attached to a visible window yet). `main_screen_scale`
        // hits `UIScreen.mainScreen.scale` and returns the device's
        // real scale (3.0 on Retina iPhones). Without this, Slint's
        // software renderer paints at 1x into a buffer treated as
        // 3x with visibly grainy edges.
        //
        // Editor dimensions × backing-scale stay well below 2^23,
        // so the f32 mantissa loss never matters; scale ≤ 4.0 on
        // every Apple device.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let scale = truce_gui::platform::main_screen_scale() as f32;
        let _ = parent; // scale comes from the screen, not the parent view
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let phys_w = (lw as f32 * scale).round() as u32;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let phys_h = (lh as f32 * scale).round() as u32;

        // Register the truce Slint platform on this thread and
        // pre-create the MinimalSoftwareWindow Slint components will
        // attach to during the setup closure.
        //
        // Dispatch the `ScaleFactorChanged` event **before**
        // `set_size`: Slint converts its internal logical extents
        // using the current scale factor at the moment `set_size`
        // runs, so seeding a fresh window with a physical size
        // while the default 1× scale is still in effect leaves the
        // first frame's draw at 1× DPI and Core Animation upscales
        // the bitmap - visible as grainy edges on every widget.
        ensure_platform();
        let slint_window = create_slint_window();
        slint_window
            .window()
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: scale,
            });
        slint_window.set_size(slint::PhysicalSize::new(phys_w, phys_h));

        // Run the user's setup closure inside the configured
        // platform - produces the SyncFn we call each frame.
        let typed_ctx = context.with_params(Arc::clone(&self.params));
        let sync = (self.setup)(typed_ctx.clone());

        // SAFETY: UIKit main-thread only; CADisplayLink retains the
        // view as its target; ivar pin released in close() via
        // Arc::from_raw.
        let (view, link) =
            unsafe { install_editor_view::<P>(parent_ptr.cast(), lw, lh, scale, &self.inner) };
        if view.is_null() {
            return;
        }

        let inner = Inner {
            child_view: view,
            display_link: link,
            logical_w: lw,
            logical_h: lh,
            scale,
            slint_window,
            px_buf: Vec::with_capacity((phys_w * phys_h) as usize),
            rgba_buf: Vec::with_capacity((phys_w * phys_h * 4) as usize),
            sync,
            context: typed_ctx,
            params: Arc::clone(&self.params),
            last_pointer: LogicalPosition::new(-1.0, -1.0),
            pending_events: RefCell::new(Vec::with_capacity(16)),
        };
        *self.inner.lock().expect("inner mutex") = Some(inner);
    }

    fn close(&mut self) {
        let Some(inner) = self.inner.lock().expect("inner mutex").take() else {
            return;
        };
        unsafe {
            if !inner.display_link.is_null() {
                let _: () = msg_send![inner.display_link, invalidate];
                let _: () = msg_send![inner.display_link, release];
            }
            if !inner.child_view.is_null() {
                let cls: &AnyClass = msg_send![inner.child_view, class];
                let base: *const u8 = inner.child_view.cast();
                let ivar_ptr: *const *mut std::ffi::c_void =
                    base.add(ivar_offset(cls, INNER_PTR_IVAR)).cast();
                let leaked = (*ivar_ptr).cast_const().cast::<Mutex<Option<Inner<P>>>>();
                if !leaked.is_null() {
                    let _ = Arc::from_raw(leaked);
                }
                let _: () = msg_send![inner.child_view, removeFromSuperview];
            }
        }
        drop(inner);
    }
}

// UIView subclass + CADisplayLink + touch handling

const INNER_PTR_IVAR: &std::ffi::CStr = c"_truce_slint_inner_ptr";

unsafe extern "C" {
    static NSRunLoopCommonModes: *const AnyObject;
}

unsafe fn install_editor_view<P: Params + 'static>(
    parent: *mut AnyObject,
    logical_w: u32,
    logical_h: u32,
    scale: f32,
    slot: &Arc<Mutex<Option<Inner<P>>>>,
) -> (*mut AnyObject, *mut AnyObject) {
    use std::any::type_name;
    unsafe {
        let class_name_owned = format!(
            "TruceSlintiOSEditorView_{:x}",
            fnv1a_64(type_name::<Inner<P>>().as_bytes())
        );
        let class_name = std::ffi::CString::new(class_name_owned).expect("ascii");
        let uiview = AnyClass::get(c"UIView").expect("UIView missing");

        let cls: &AnyClass = if let Some(existing) = AnyClass::get(class_name.as_c_str()) {
            existing
        } else {
            let mut builder = ClassBuilder::new(class_name.as_c_str(), uiview)
                .expect("unique class per monomorphization");
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
        };

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
        let _: () = msg_send![view, setUserInteractionEnabled: true];
        let _: () = msg_send![view, setContentScaleFactor: f64::from(scale)];

        // The view's default `CALayer` is fine - we blit a CGImage
        // into `layer.contents` each tick. No CAMetalLayer needed
        // (Slint's software renderer is CPU-only).

        // Pin the Arc into the ivar - released in close().
        let leaked: *const Mutex<Option<Inner<P>>> = Arc::into_raw(Arc::clone(slot));
        let base = view.cast::<u8>();
        let ivar_ptr: *mut *mut std::ffi::c_void =
            base.add(ivar_offset(cls, INNER_PTR_IVAR)).cast();
        *ivar_ptr = leaked as *mut std::ffi::c_void;

        let _: () = msg_send![parent, addSubview: view];

        // CADisplayLink → tick: every refresh.
        let dl_cls = AnyClass::get(c"CADisplayLink").expect("CADisplayLink missing");
        let link: *mut AnyObject =
            msg_send![dl_cls, displayLinkWithTarget: view, selector: sel!(tick:)];
        if link.is_null() {
            return (view, std::ptr::null_mut());
        }
        let _: () = msg_send![link, retain];
        let run_loop_cls = AnyClass::get(c"NSRunLoop").expect("NSRunLoop missing");
        let main: *mut AnyObject = msg_send![run_loop_cls, mainRunLoop];
        let mode: *const AnyObject = NSRunLoopCommonModes;
        let _: () = msg_send![link, addToRunLoop: main, forMode: mode];

        (view, link)
    }
}

unsafe fn borrow_inner_arc<P: Params + 'static>(
    self_: &AnyObject,
) -> Option<Arc<Mutex<Option<Inner<P>>>>> {
    unsafe {
        let cls: &AnyClass = msg_send![self_, class];
        let base: *const u8 = std::ptr::from_ref::<AnyObject>(self_).cast();
        let ivar_ptr: *const *mut std::ffi::c_void =
            base.add(ivar_offset(cls, INNER_PTR_IVAR)).cast();
        let leaked = (*ivar_ptr).cast_const().cast::<Mutex<Option<Inner<P>>>>();
        if leaked.is_null() {
            return None;
        }
        let arc = Arc::from_raw(leaked);
        let cloned = Arc::clone(&arc);
        let _ = Arc::into_raw(arc);
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
        let Ok(mut guard) = arc.lock() else { return };
        let Some(inner) = guard.as_mut() else { return };
        run_frame(inner);
    }
}

fn run_frame<P: Params + 'static>(inner: &mut Inner<P>) {
    // Drain pending touch events through the Slint window.
    let events: Vec<WindowEvent> = inner.pending_events.borrow_mut().drain(..).collect();
    for ev in events {
        inner.slint_window.window().dispatch_event(ev);
    }
    // Drive host → UI param sync each frame.
    (inner.sync)(&inner.context);

    // Slint's event loop needs a tick to flush layout / property
    // changes. `slint::platform::update_timers_and_animations`
    // would do it for animations; for property + layout
    // propagation, the call below is enough on the desktop path.
    let _ = inner.params; // kept alive for the duration

    // Same physical-pixel cast rationale as `SlintEditor::open`.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let phys_w = (inner.logical_w as f32 * inner.scale).round() as u32;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let phys_h = (inner.logical_h as f32 * inner.scale).round() as u32;
    render_to_rgba(
        &inner.slint_window,
        phys_w,
        phys_h,
        &mut inner.px_buf,
        &mut inner.rgba_buf,
    );

    unsafe {
        blit_pixmap_to_layer(inner.child_view, phys_w, phys_h, &inner.rgba_buf);
    }
}

unsafe extern "C" fn touches_began<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe { dispatch_touch::<P>(self_, touches, TouchPhase::Began) }
}

unsafe extern "C" fn touches_moved<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe { dispatch_touch::<P>(self_, touches, TouchPhase::Moved) }
}

unsafe extern "C" fn touches_ended<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe { dispatch_touch::<P>(self_, touches, TouchPhase::Ended) }
}

unsafe extern "C" fn touches_cancelled<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe { dispatch_touch::<P>(self_, touches, TouchPhase::Ended) }
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
        let Ok(mut guard) = arc.lock() else { return };
        let Some(inner) = guard.as_mut() else { return };

        let touch: *mut AnyObject = msg_send![touches, anyObject];
        if touch.is_null() {
            return;
        }
        let view_ptr: *mut AnyObject = std::ptr::from_ref::<AnyObject>(self_).cast_mut();
        let pt: NSPoint = msg_send![touch, locationInView: view_ptr];
        #[allow(clippy::cast_possible_truncation)]
        let pos = LogicalPosition::new(pt.x as f32, pt.y as f32);
        inner.last_pointer = pos;
        let mut q = inner.pending_events.borrow_mut();
        q.push(WindowEvent::PointerMoved { position: pos });
        match phase {
            TouchPhase::Began => q.push(WindowEvent::PointerPressed {
                position: pos,
                button: PointerEventButton::Left,
            }),
            TouchPhase::Ended => {
                q.push(WindowEvent::PointerReleased {
                    position: pos,
                    button: PointerEventButton::Left,
                });
                q.push(WindowEvent::PointerExited);
            }
            TouchPhase::Moved => {}
        }
    }
}

// CGImage blit - same shape as truce-gui::editor_ios. Duplicated
// rather than shared because the helper is < 50 lines and lifting it
// into a separate crate would force every alt-GUI backend to depend
// on truce-gui, which we explicitly avoid.

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
const K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST: u32 = 1;
const K_CG_RENDERING_INTENT_DEFAULT: i32 = 0;

unsafe fn blit_pixmap_to_layer(view: *mut AnyObject, width: u32, height: u32, rgba: &[u8]) {
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
