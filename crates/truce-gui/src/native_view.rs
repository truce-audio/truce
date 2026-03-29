//! Native macOS NSView for AAX plugin editors.
//!
//! No timer, no CVDisplayLink — the host's `idle()` callback drives rendering.
//! After blitting a new CGImage into the view's ivar, we call `setNeedsDisplay:`
//! and AppKit schedules `drawRect:` on its own display cycle.
//!
//! Architecture:
//! - Host calls `idle()` → CPU render → CGImage stored in view ivar
//! - `setNeedsDisplay:YES` called after blit
//! - AppKit calls `drawRect:` → draws CGImage via CGContextDrawImage
//! - Mouse events handled by the NSView's ObjC callbacks
//!
//! No layer backing (`setWantsLayer: NO`) to avoid autoreleased objects during
//! dealloc that cause use-after-free when Pro Tools drains its per-callout ARP.

#[cfg(target_os = "macos")]
use std::ffi::c_void;

#[cfg(target_os = "macos")]
use cocoa::base::{id, nil, BOOL, YES};
#[cfg(target_os = "macos")]
use cocoa::foundation::{NSPoint, NSRect, NSSize};
#[cfg(target_os = "macos")]
use objc::declare::ClassDecl;
#[cfg(target_os = "macos")]
use objc::runtime::{Class, Object, Sel};
#[cfg(target_os = "macos")]
use objc::{class, msg_send, sel, sel_impl};

// ---------------------------------------------------------------------------
// Ivar names
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
const STATE_IVAR: &str = "truce_native_view_state";



// ---------------------------------------------------------------------------
// Public callback table
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub struct NativeViewCallbacks {
    pub ctx: *mut c_void,
    pub on_mouse_moved: unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32),
    pub on_mouse_down: unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32),
    pub on_mouse_up: unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32),
    pub on_scroll: unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32, dy: f32),
    pub on_mouse_exited: unsafe extern "C" fn(ctx: *mut c_void),
    pub drop_ctx: unsafe extern "C" fn(ctx: *mut c_void),
}

#[cfg(target_os = "macos")]
struct NativeViewState {
    callbacks: NativeViewCallbacks,
}

// ---------------------------------------------------------------------------
// NativeView
// ---------------------------------------------------------------------------

/// Handle to a native macOS NSView for AAX plugin editors.
/// Rendering is driven by the host's idle callback — no timer or display link.
#[cfg(target_os = "macos")]
pub struct NativeView {
    ns_view: id,
}

#[cfg(target_os = "macos")]
unsafe impl Send for NativeView {}

#[cfg(target_os = "macos")]
impl NativeView {
    pub fn close(&mut self) {
        unsafe {
            extern "C" {
                fn objc_autoreleasePoolPush() -> *mut c_void;
                fn objc_autoreleasePoolPop(pool: *mut c_void);
            }

            if !self.ns_view.is_null() {
                // 1. Null out state — makes all ObjC callbacks no-op
                let state_ptr: *mut c_void = *(*self.ns_view).get_ivar(STATE_IVAR);
                if !state_ptr.is_null() {
                    let state = Box::from_raw(state_ptr as *mut NativeViewState);
                    (state.callbacks.drop_ctx)(state.callbacks.ctx);
                    drop(state);
                    (*self.ns_view).set_ivar(STATE_IVAR, std::ptr::null_mut::<c_void>());
                }

                // 2. Clear layer contents (releases the CGImage the layer holds)
                {
                    let layer: id = msg_send![self.ns_view, layer];
                    if !layer.is_null() {
                        let _: () = msg_send![layer, setContents: nil];
                    }
                }

                // 3. Strip view of all interactive state and remove from
                //    the view hierarchy.
                let pool = objc_autoreleasePoolPush();

                let window: id = msg_send![self.ns_view, window];
                if !window.is_null() {
                    let _: () = msg_send![window, makeFirstResponder: nil];
                }
                let tracking_areas: id = msg_send![self.ns_view, trackingAreas];
                let count: usize = msg_send![tracking_areas, count];
                for i in (0..count).rev() {
                    let area: id = msg_send![tracking_areas, objectAtIndex: i];
                    let _: () = msg_send![self.ns_view, removeTrackingArea: area];
                }
                let _: () = msg_send![self.ns_view, removeFromSuperview];
                let _: () = msg_send![self.ns_view, release];
                self.ns_view = nil;

                objc_autoreleasePoolPop(pool);
            }
        }
    }

    pub fn ns_view_ptr(&self) -> *mut c_void {
        self.ns_view as *mut c_void
    }

    pub fn state_ctx(&self) -> *mut c_void {
        if self.ns_view.is_null() {
            return std::ptr::null_mut();
        }
        unsafe {
            let state_ptr: *mut c_void = *(*self.ns_view).get_ivar(STATE_IVAR);
            if state_ptr.is_null() {
                return std::ptr::null_mut();
            }
            let state = &*(state_ptr as *const NativeViewState);
            state.callbacks.ctx
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for NativeView {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------------------
// open()
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub unsafe fn open(
    parent: *mut c_void,
    width: f64,
    height: f64,
    callbacks: NativeViewCallbacks,
) -> NativeView {
    extern "C" {
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }

    let pool = objc_autoreleasePoolPush();

    let view_class = create_view_class();

    let ns_view: id = msg_send![view_class, alloc];
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
    let ns_view: id = msg_send![ns_view, initWithFrame: frame];

    // Layer-backed view — we set contents directly via [layer setContents:]
    // (same as JUCE). NSViewLayerContentsRedrawNever (0) tells AppKit to
    // never call drawRect: — we manage layer contents ourselves.
    let _: () = msg_send![ns_view, setWantsLayer: YES];
    let _: () = msg_send![ns_view, setLayerContentsRedrawPolicy: 0isize]; // Never

    let state = Box::new(NativeViewState { callbacks });
    let state_ptr = Box::into_raw(state) as *mut c_void;
    (*ns_view).set_ivar(STATE_IVAR, state_ptr);

    let parent_view = parent as id;
    let _: () = msg_send![parent_view, addSubview: ns_view];

    let window: id = msg_send![ns_view, window];
    if !window.is_null() {
        let _: () = msg_send![window, setAcceptsMouseMovedEvents: YES];
        let _: () = msg_send![window, makeFirstResponder: ns_view];
    }

    setup_tracking_area(ns_view);

    objc_autoreleasePoolPop(pool);

    NativeView { ns_view }
}

// ---------------------------------------------------------------------------
// NSView subclass creation
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
unsafe fn create_view_class() -> &'static Class {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let class_name = format!(
        "TruceNativeView_{}",
        COUNTER.fetch_add(1, Ordering::Relaxed),
    );
    let mut class = ClassDecl::new(&class_name, class!(NSView))
        .expect("failed to create TruceNativeView class");

    class.add_ivar::<*mut c_void>(STATE_IVAR);
    class.add_method(sel!(acceptsFirstResponder), returns_yes as extern "C" fn(&Object, Sel) -> BOOL);
    class.add_method(sel!(isFlipped), returns_yes as extern "C" fn(&Object, Sel) -> BOOL);
    class.add_method(sel!(acceptsFirstMouse:), accepts_first_mouse as extern "C" fn(&Object, Sel, id) -> BOOL);

    class.add_method(sel!(mouseMoved:), mouse_moved as extern "C" fn(&Object, Sel, id));
    class.add_method(sel!(mouseDragged:), mouse_moved as extern "C" fn(&Object, Sel, id));
    class.add_method(sel!(rightMouseDragged:), mouse_moved as extern "C" fn(&Object, Sel, id));
    class.add_method(sel!(otherMouseDragged:), mouse_moved as extern "C" fn(&Object, Sel, id));
    class.add_method(sel!(mouseDown:), mouse_down as extern "C" fn(&Object, Sel, id));
    class.add_method(sel!(mouseUp:), mouse_up as extern "C" fn(&Object, Sel, id));
    class.add_method(sel!(scrollWheel:), scroll_wheel as extern "C" fn(&Object, Sel, id));
    class.add_method(sel!(mouseEntered:), mouse_entered as extern "C" fn(&Object, Sel, id));
    class.add_method(sel!(mouseExited:), mouse_exited as extern "C" fn(&Object, Sel, id));

    class.add_method(sel!(updateTrackingAreas), update_tracking_areas as extern "C" fn(&Object, Sel));
    class.add_method(sel!(viewWillMoveToWindow:), view_will_move_to_window as extern "C" fn(&Object, Sel, id));
    class.add_method(sel!(dealloc), dealloc as extern "C" fn(&mut Object, Sel));

    class.register()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
unsafe fn get_state(this: &Object) -> Option<&NativeViewState> {
    let ptr: *mut c_void = *this.get_ivar(STATE_IVAR);
    if ptr.is_null() { None } else { Some(&*(ptr as *const NativeViewState)) }
}

#[cfg(target_os = "macos")]
unsafe fn setup_tracking_area(ns_view: id) {
    let options: usize = 0x01 | 0x02 | 0x40 | 0x200 | 0x400;
    let bounds: NSRect = msg_send![ns_view, bounds];
    let cls = Class::get("NSTrackingArea").unwrap();
    let tracking_area: id = msg_send![cls, alloc];
    let tracking_area: id = msg_send![
        tracking_area,
        initWithRect: bounds
        options: options
        owner: ns_view
        userInfo: nil
    ];
    let _: () = msg_send![ns_view, addTrackingArea: tracking_area];
}

// ---------------------------------------------------------------------------
// ObjC callbacks
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
extern "C" fn returns_yes(_this: &Object, _sel: Sel) -> BOOL { YES }

#[cfg(target_os = "macos")]
extern "C" fn accepts_first_mouse(_this: &Object, _sel: Sel, _event: id) -> BOOL { YES }

#[cfg(target_os = "macos")]
extern "C" fn mouse_moved(this: &Object, _sel: Sel, event: id) {
    let Some(state) = (unsafe { get_state(this) }) else { return };
    let point: NSPoint = unsafe {
        let p: NSPoint = msg_send![event, locationInWindow];
        msg_send![this, convertPoint: p fromView: nil]
    };
    unsafe { (state.callbacks.on_mouse_moved)(state.callbacks.ctx, point.x as f32, point.y as f32) };
}

#[cfg(target_os = "macos")]
extern "C" fn mouse_down(this: &Object, _sel: Sel, event: id) {
    let Some(state) = (unsafe { get_state(this) }) else { return };
    let point: NSPoint = unsafe {
        let p: NSPoint = msg_send![event, locationInWindow];
        msg_send![this, convertPoint: p fromView: nil]
    };
    unsafe { (state.callbacks.on_mouse_down)(state.callbacks.ctx, point.x as f32, point.y as f32) };
}

#[cfg(target_os = "macos")]
extern "C" fn mouse_up(this: &Object, _sel: Sel, event: id) {
    let Some(state) = (unsafe { get_state(this) }) else { return };
    let point: NSPoint = unsafe {
        let p: NSPoint = msg_send![event, locationInWindow];
        msg_send![this, convertPoint: p fromView: nil]
    };
    unsafe { (state.callbacks.on_mouse_up)(state.callbacks.ctx, point.x as f32, point.y as f32) };
}

#[cfg(target_os = "macos")]
extern "C" fn scroll_wheel(this: &Object, _sel: Sel, event: id) {
    let Some(state) = (unsafe { get_state(this) }) else { return };
    let point: NSPoint = unsafe {
        let p: NSPoint = msg_send![event, locationInWindow];
        msg_send![this, convertPoint: p fromView: nil]
    };
    let dy: f64 = unsafe { msg_send![event, scrollingDeltaY] };
    let has_precise: BOOL = unsafe { msg_send![event, hasPreciseScrollingDeltas] };
    let dy = if has_precise != YES { dy * 10.0 } else { dy };
    unsafe { (state.callbacks.on_scroll)(state.callbacks.ctx, point.x as f32, point.y as f32, dy as f32) };
}

#[cfg(target_os = "macos")]
extern "C" fn mouse_entered(_this: &Object, _sel: Sel, _event: id) {}

#[cfg(target_os = "macos")]
extern "C" fn mouse_exited(this: &Object, _sel: Sel, _event: id) {
    let Some(state) = (unsafe { get_state(this) }) else { return };
    unsafe { (state.callbacks.on_mouse_exited)(state.callbacks.ctx) };
}

#[cfg(target_os = "macos")]
extern "C" fn update_tracking_areas(this: &Object, _sel: Sel) {
    unsafe {
        let tracking_areas: id = msg_send![this, trackingAreas];
        let count: usize = msg_send![tracking_areas, count];
        for i in (0..count).rev() {
            let area: id = msg_send![tracking_areas, objectAtIndex: i];
            let _: () = msg_send![this, removeTrackingArea: area];
        }
        if get_state(this).is_some() {
            setup_tracking_area(this as *const Object as id);
        }
        let superclass: &Class = msg_send![this, superclass];
        let _: () = msg_send![super(this, superclass), updateTrackingAreas];
    }
}

#[cfg(target_os = "macos")]
extern "C" fn view_will_move_to_window(this: &Object, _sel: Sel, new_window: id) {
    unsafe {
        if !new_window.is_null() {
            let _: () = msg_send![new_window, setAcceptsMouseMovedEvents: YES];
            let _: () = msg_send![new_window, makeFirstResponder: this];
        }
        let superclass: &Class = msg_send![this, superclass];
        let _: () = msg_send![super(this, superclass), viewWillMoveToWindow: new_window];
    }
}

#[cfg(target_os = "macos")]
extern "C" fn dealloc(this: &mut Object, _sel: Sel) {
    unsafe {
        extern "C" {
            fn objc_autoreleasePoolPush() -> *mut c_void;
            fn objc_autoreleasePoolPop(pool: *mut c_void);
        }
        // Clean up any remaining Rust state (normally already cleared by close())
        let state_ptr: *mut c_void = *this.get_ivar(STATE_IVAR);
        if !state_ptr.is_null() {
            let state = Box::from_raw(state_ptr as *mut NativeViewState);
            (state.callbacks.drop_ctx)(state.callbacks.ctx);
            drop(state);
        }
        // Wrap [super dealloc] in @autoreleasepool so any autoreleased
        // objects created by NSView's internal teardown are drained here,
        // not in the host's per-callout ARP.
        let pool = objc_autoreleasePoolPush();
        let superclass: &Class = msg_send![this, superclass];
        let _: () = msg_send![super(this, superclass), dealloc];
        objc_autoreleasePoolPop(pool);
    }
}
