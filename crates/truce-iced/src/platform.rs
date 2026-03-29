//! Platform view creation and management for iced embedding.
//!
//! On macOS, creates a child NSView via `shim/macos_iced_view.m`.
//! The iced/wgpu compositor handles Metal layer creation.

use std::ffi::c_void;

/// Callbacks from the platform view into Rust.
#[repr(C)]
pub struct IcedViewCallbacks {
    pub setup: Option<unsafe extern "C" fn(ctx: *mut c_void, ns_view: *mut c_void)>,
    pub render: Option<unsafe extern "C" fn(ctx: *mut c_void)>,
    pub mouse_down: Option<unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32)>,
    pub mouse_dragged: Option<unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32)>,
    pub mouse_up: Option<unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32)>,
    pub scroll: Option<unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32, delta_y: f32)>,
    pub double_click: Option<unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32)>,
    pub mouse_moved: Option<unsafe extern "C" fn(ctx: *mut c_void, x: f32, y: f32) -> u8>,
}

#[cfg(target_os = "macos")]
extern "C" {
    fn truce_iced_view_create(
        parent: *mut c_void,
        width: u32,
        height: u32,
        ctx: *mut c_void,
        callbacks: *const IcedViewCallbacks,
        no_timer: i32,
    ) -> *mut c_void;

    fn truce_iced_view_tick(view_handle: *mut c_void);

    fn truce_iced_view_destroy(view_handle: *mut c_void);
}

/// Opaque handle to a platform view for iced rendering.
pub struct IcedPlatformView {
    handle: *mut c_void,
}

unsafe impl Send for IcedPlatformView {}

impl IcedPlatformView {
    /// Create an iced platform view as a child of the given parent window.
    ///
    /// If `no_timer` is true, the repaint timer is not started — the host
    /// must call `tick()` from its idle callback instead.
    ///
    /// # Safety
    /// `parent` must be a valid NSView*. `ctx` and `callbacks` must remain
    /// valid for the lifetime of the view.
    #[cfg(target_os = "macos")]
    pub unsafe fn new(
        parent: *mut c_void,
        width: u32,
        height: u32,
        ctx: *mut c_void,
        callbacks: &IcedViewCallbacks,
        no_timer: bool,
    ) -> Option<Self> {
        let handle = truce_iced_view_create(
            parent, width, height, ctx, callbacks, no_timer as i32,
        );
        if handle.is_null() {
            None
        } else {
            Some(IcedPlatformView { handle })
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub unsafe fn new(
        _parent: *mut c_void,
        _width: u32,
        _height: u32,
        _ctx: *mut c_void,
        _callbacks: &IcedViewCallbacks,
        _no_timer: bool,
    ) -> Option<Self> {
        None
    }

    /// Drive one render tick. Only needed when `no_timer` was set in `new()`.
    #[cfg(target_os = "macos")]
    pub fn tick(&self) {
        unsafe { truce_iced_view_tick(self.handle); }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn tick(&self) {}
}

impl Drop for IcedPlatformView {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            #[cfg(target_os = "macos")]
            unsafe {
                truce_iced_view_destroy(self.handle);
            }
        }
    }
}
