//! Platform window bridging for baseview.
//!
//! Bridges truce's `RawWindowHandle` to baseview's `HasRawWindowHandle`
//! (raw-window-handle 0.5), and provides scale factor querying.

use raw_window_handle::{HasRawWindowHandle, RawWindowHandle as RwhRawWindowHandle};
use truce_core::editor::RawWindowHandle;

/// Newtype bridging truce's `RawWindowHandle` to baseview's
/// `HasRawWindowHandle` (raw-window-handle 0.5).
pub struct ParentWindow(pub RawWindowHandle);

unsafe impl HasRawWindowHandle for ParentWindow {
    fn raw_window_handle(&self) -> RwhRawWindowHandle {
        match self.0 {
            RawWindowHandle::AppKit(ptr) => {
                let mut handle = raw_window_handle::AppKitWindowHandle::empty();
                handle.ns_view = ptr;
                RwhRawWindowHandle::AppKit(handle)
            }
            RawWindowHandle::Win32(ptr) => {
                let mut handle = raw_window_handle::Win32WindowHandle::empty();
                handle.hwnd = ptr;
                RwhRawWindowHandle::Win32(handle)
            }
            RawWindowHandle::X11(window_id) => {
                let mut handle = raw_window_handle::XlibWindowHandle::empty();
                handle.window = window_id as u32;
                RwhRawWindowHandle::Xlib(handle)
            }
        }
    }
}

/// Query the backing scale factor from the parent NSView's window.
#[cfg(target_os = "macos")]
pub fn query_backing_scale(parent: &RawWindowHandle) -> f64 {
    use objc::{msg_send, sel, sel_impl};

    let ns_view_ptr = match parent {
        RawWindowHandle::AppKit(ptr) => *ptr,
        _ => return 1.0,
    };

    if ns_view_ptr.is_null() {
        return 1.0;
    }

    unsafe {
        let ns_view = ns_view_ptr as cocoa::base::id;
        let window: cocoa::base::id = msg_send![ns_view, window];
        let scale: f64 = if !window.is_null() {
            msg_send![window, backingScaleFactor]
        } else {
            let screen: cocoa::base::id = msg_send![objc::class!(NSScreen), mainScreen];
            if !screen.is_null() {
                msg_send![screen, backingScaleFactor]
            } else {
                2.0
            }
        };
        if scale < 1.0 { 1.0 } else { scale }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn query_backing_scale(_parent: &RawWindowHandle) -> f64 {
    1.0
}

/// Bridge a baseview raw-window-handle 0.5 `AppKitWindowHandle` to a
/// wgpu-compatible `SurfaceTargetUnsafe` using rwh 0.6 types.
///
/// # Safety
/// The window handle must be valid for the lifetime of the returned surface.
pub unsafe fn create_wgpu_surface(
    instance: &wgpu::Instance,
    window: &baseview::Window,
) -> Option<wgpu::Surface<'static>> {
    let rwh = window.raw_window_handle();
    let surface_target = match rwh {
        #[cfg(target_os = "macos")]
        RwhRawWindowHandle::AppKit(handle) => {
            let ns_view = handle.ns_view;
            if ns_view.is_null() {
                return None;
            }
            // Construct rwh 0.6 handles for wgpu
            let rwh6_window = wgpu::rwh::RawWindowHandle::AppKit(
                wgpu::rwh::AppKitWindowHandle::new(
                    std::ptr::NonNull::new(ns_view)?,
                ),
            );
            let rwh6_display = wgpu::rwh::RawDisplayHandle::AppKit(
                wgpu::rwh::AppKitDisplayHandle::new(),
            );
            wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: rwh6_display,
                raw_window_handle: rwh6_window,
            }
        }
        #[cfg(target_os = "windows")]
        RwhRawWindowHandle::Win32(handle) => {
            let hwnd = handle.hwnd;
            if hwnd.is_null() {
                return None;
            }
            let rwh6_window = wgpu::rwh::RawWindowHandle::Win32(
                wgpu::rwh::Win32WindowHandle::new(
                    std::num::NonZero::new(hwnd as isize)?,
                ),
            );
            let rwh6_display = wgpu::rwh::RawDisplayHandle::Windows(
                wgpu::rwh::WindowsDisplayHandle::new(),
            );
            wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: rwh6_display,
                raw_window_handle: rwh6_window,
            }
        }
        #[cfg(target_os = "linux")]
        RwhRawWindowHandle::Xlib(handle) => {
            let rwh6_window = wgpu::rwh::RawWindowHandle::Xlib(
                wgpu::rwh::XlibWindowHandle::new(handle.window),
            );
            // X11 display handle — baseview doesn't expose it via rwh 0.5,
            // but wgpu can often infer it. Use a null display for now.
            let rwh6_display = wgpu::rwh::RawDisplayHandle::Xlib(
                wgpu::rwh::XlibDisplayHandle::new(None, 0),
            );
            wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: rwh6_display,
                raw_window_handle: rwh6_window,
            }
        }
        _ => return None,
    };

    instance.create_surface_unsafe(surface_target).ok()
}
