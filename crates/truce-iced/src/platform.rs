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
    ) -> *mut c_void;

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
    ) -> Option<Self> {
        let handle = truce_iced_view_create(parent, width, height, ctx, callbacks);
        if handle.is_null() {
            None
        } else {
            Some(IcedPlatformView { handle })
        }
    }

    /// Non-macOS stub. Always returns `None` — iced child-window
    /// integration is currently macOS-only (the windowed path uses
    /// baseview directly on Windows / Linux).
    ///
    /// # Safety
    /// Trivially safe (no pointers dereferenced); `unsafe` is kept on
    /// the signature only to match the macOS variant so call sites
    /// don't need a `cfg`-gated `unsafe { ... }` block.
    #[cfg(not(target_os = "macos"))]
    pub unsafe fn new(
        _parent: *mut c_void,
        _width: u32,
        _height: u32,
        _ctx: *mut c_void,
        _callbacks: &IcedViewCallbacks,
    ) -> Option<Self> {
        None
    }
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
