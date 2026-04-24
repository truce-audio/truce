//! Platform window bridging for baseview.
//!
//! Bridges truce's `RawWindowHandle` to baseview's `HasRawWindowHandle`
//! (raw-window-handle 0.5), and provides scale factor querying.

// `HasRawDisplayHandle` / `RwhRawDisplayHandle` are only touched on
// the Linux (X11) arm of `HasRawWindowHandle for ParentWindow`;
// silence the macOS/Windows dead-import warning.
#[allow(unused_imports)]
use raw_window_handle::{
    HasRawDisplayHandle, HasRawWindowHandle, RawDisplayHandle as RwhRawDisplayHandle,
    RawWindowHandle as RwhRawWindowHandle,
};
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
                // rwh 0.5 field type is c_ulong: u64 on Linux/macOS, u32 on Windows.
                handle.window = window_id as _;
                RwhRawWindowHandle::Xlib(handle)
            }
        }
    }
}

/// Bridge a baseview raw-window-handle 0.5 to a wgpu surface.
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
            let rwh6_window = wgpu::rwh::RawWindowHandle::AppKit(
                wgpu::rwh::AppKitWindowHandle::new(std::ptr::NonNull::new(ns_view)?),
            );
            let rwh6_display =
                wgpu::rwh::RawDisplayHandle::AppKit(wgpu::rwh::AppKitDisplayHandle::new());
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
            let rwh6_window = wgpu::rwh::RawWindowHandle::Win32(wgpu::rwh::Win32WindowHandle::new(
                std::num::NonZero::new(hwnd as isize)?,
            ));
            let rwh6_display =
                wgpu::rwh::RawDisplayHandle::Windows(wgpu::rwh::WindowsDisplayHandle::new());
            wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: rwh6_display,
                raw_window_handle: rwh6_window,
            }
        }
        #[cfg(target_os = "linux")]
        RwhRawWindowHandle::Xlib(handle) => {
            let display_handle = match window.raw_display_handle() {
                RwhRawDisplayHandle::Xlib(d) => d,
                _ => return None,
            };
            let display_ptr = std::ptr::NonNull::new(display_handle.display);
            let rwh6_window =
                wgpu::rwh::RawWindowHandle::Xlib(wgpu::rwh::XlibWindowHandle::new(handle.window));
            let rwh6_display = wgpu::rwh::RawDisplayHandle::Xlib(
                wgpu::rwh::XlibDisplayHandle::new(display_ptr, display_handle.screen),
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
