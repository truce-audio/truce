//! Platform-specific view creation and management.
//!
//! On macOS, creates a child NSView via the ObjC shim in `shim/macos_view.m`.
//! On other platforms, this is a stub (no-op).

use std::ffi::c_void;

/// Callbacks from the platform view into Rust.
#[repr(C)]
pub struct ViewCallbacks {
    pub render:
        Option<unsafe extern "C" fn(ctx: *mut c_void, w: *mut u32, h: *mut u32) -> *const u8>,
    pub mouse_down: Option<unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32)>,
    pub mouse_dragged: Option<unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32)>,
    pub mouse_up: Option<unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32)>,
    pub scroll: Option<unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32, delta_y: f32)>,
    pub double_click: Option<unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32)>,
    pub mouse_moved: Option<unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32) -> u8>,
}

#[cfg(target_os = "macos")]
extern "C" {
    fn truce_view_create(
        parent: *mut c_void,
        width: u32,
        height: u32,
        ctx: *mut c_void,
        callbacks: *const ViewCallbacks,
    ) -> *mut c_void;

    fn truce_view_destroy(view_handle: *mut c_void);

    /// Returns the main screen's backing scale factor (Retina = 2.0).
    pub fn truce_platform_backing_scale() -> f64;
}

/// Opaque handle to a platform view.
pub struct PlatformView {
    handle: *mut c_void,
}

// Safety: the handle is only accessed from the main/GUI thread
unsafe impl Send for PlatformView {}

impl PlatformView {
    /// Create a platform view as a child of the given parent window.
    ///
    /// # Safety
    /// `parent` must be a valid NSView* (macOS), HWND (Windows), or X11 Window.
    /// `ctx` must remain valid for the lifetime of the view.
    /// `callbacks` must remain valid for the lifetime of the view.
    #[cfg(target_os = "macos")]
    pub unsafe fn new(
        parent: *mut c_void,
        width: u32,
        height: u32,
        ctx: *mut c_void,
        callbacks: &ViewCallbacks,
    ) -> Option<Self> {
        let handle = truce_view_create(parent, width, height, ctx, callbacks);
        if handle.is_null() {
            None
        } else {
            Some(PlatformView { handle })
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub unsafe fn new(
        _parent: *mut c_void,
        _width: u32,
        _height: u32,
        _ctx: *mut c_void,
        _callbacks: &ViewCallbacks,
    ) -> Option<Self> {
        None // Not implemented on this platform yet
    }
}

impl Drop for PlatformView {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            #[cfg(target_os = "macos")]
            unsafe {
                truce_view_destroy(self.handle);
            }
        }
    }
}
