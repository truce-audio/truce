//! Platform window bridging for baseview.
//!
//! Bridges truce's `RawWindowHandle` to baseview's `HasRawWindowHandle`
//! (raw-window-handle 0.5), and provides scale factor querying and
//! wgpu surface creation.

// `HasRawDisplayHandle` / `RwhRawDisplayHandle` are only touched on
// the Linux (X11) arm of `HasRawWindowHandle for ParentWindow`;
// silence the macOS/Windows dead-import warning.
#[allow(unused_imports)]
use raw_window_handle::{
    HasRawDisplayHandle, HasRawWindowHandle, RawDisplayHandle as RwhRawDisplayHandle,
    RawWindowHandle as RwhRawWindowHandle,
};
use truce_core::editor::RawWindowHandle;

#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU64, Ordering};

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

#[cfg(target_os = "windows")]
pub fn query_backing_scale(parent: &RawWindowHandle) -> f64 {
    let hwnd = match parent {
        RawWindowHandle::Win32(ptr) => *ptr,
        _ => return 1.0,
    };
    win32_dpi_scale(hwnd)
}

#[cfg(target_os = "linux")]
pub fn query_backing_scale(_parent: &RawWindowHandle) -> f64 {
    main_screen_scale()
}

/// Query the main screen's backing scale factor (no parent window needed).
#[cfg(target_os = "macos")]
pub fn main_screen_scale() -> f64 {
    use objc::{msg_send, sel, sel_impl};
    unsafe {
        let screen: cocoa::base::id = msg_send![objc::class!(NSScreen), mainScreen];
        if !screen.is_null() {
            let scale: f64 = msg_send![screen, backingScaleFactor];
            if scale < 1.0 { 1.0 } else { scale }
        } else {
            1.0
        }
    }
}

#[cfg(target_os = "windows")]
pub fn main_screen_scale() -> f64 {
    win32_dpi_scale(std::ptr::null_mut())
}

/// Cached display scale factor on Linux, stored as f64 bits. Zero means unset.
///
/// Linux has no safe synchronous DPI query from plugin code — the authoritative
/// value is read by baseview internally (from `Xft.dpi` with a screen-geometry
/// fallback) and delivered via `WindowEvent::Resized::info.scale()` once the
/// window is live. We cache the first value an editor sees there so that later
/// pre-window `main_screen_scale()` calls (e.g. the next editor's `::new`)
/// return something useful instead of 1.0.
#[cfg(target_os = "linux")]
static LINUX_SCALE_BITS: AtomicU64 = AtomicU64::new(0);

/// Record the display scale factor observed from baseview on Linux. Editors
/// should call this from their `WindowEvent::Resized` handlers so subsequent
/// pre-window queries match what baseview is delivering. No-op on non-Linux.
pub fn note_linux_scale_factor(_scale: f64) {
    #[cfg(target_os = "linux")]
    {
        if _scale.is_finite() && _scale > 0.0 {
            LINUX_SCALE_BITS.store(_scale.to_bits(), Ordering::Relaxed);
        }
    }
}

#[cfg(target_os = "linux")]
pub fn main_screen_scale() -> f64 {
    // Priority: TRUCE_SCALE env var (dev/test override) → cached scale
    // observed from baseview → 1.0 fallback. No side-channel Xlib calls —
    // those crashed inside NVIDIA's Vulkan driver when invoked from the
    // render thread (see docs/internal/linux.md, DPI section).
    if let Ok(s) = std::env::var("TRUCE_SCALE") {
        if let Ok(v) = s.parse::<f64>() {
            if v.is_finite() && v > 0.0 {
                return v;
            }
        }
    }
    let bits = LINUX_SCALE_BITS.load(Ordering::Relaxed);
    if bits == 0 {
        return 1.0;
    }
    let v = f64::from_bits(bits);
    if v.is_finite() && v > 0.0 { v } else { 1.0 }
}

/// Query the DPI scale factor on Windows.
/// If `hwnd` is non-null, queries per-window DPI; otherwise queries the system DPI.
#[cfg(target_os = "windows")]
fn win32_dpi_scale(hwnd: *mut std::ffi::c_void) -> f64 {
    // Default DPI is 96; scale = actual_dpi / 96.
    const DEFAULT_DPI: u32 = 96;

    extern "system" {
        fn GetDpiForWindow(hwnd: *mut std::ffi::c_void) -> u32;
        fn GetDpiForSystem() -> u32;
    }

    let dpi = if !hwnd.is_null() {
        let d = unsafe { GetDpiForWindow(hwnd) };
        if d == 0 { unsafe { GetDpiForSystem() } } else { d }
    } else {
        unsafe { GetDpiForSystem() }
    };

    if dpi == 0 { 1.0 } else { dpi as f64 / DEFAULT_DPI as f64 }
}

/// Bridge a baseview raw-window-handle 0.5 to a wgpu-compatible
/// `SurfaceTargetUnsafe` using rwh 0.6 types.
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
            let display_handle = match window.raw_display_handle() {
                RwhRawDisplayHandle::Xlib(d) => d,
                _ => return None,
            };
            let display_ptr = std::ptr::NonNull::new(display_handle.display);
            let rwh6_window = wgpu::rwh::RawWindowHandle::Xlib(
                wgpu::rwh::XlibWindowHandle::new(handle.window),
            );
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
