//! Platform bridging: custom Slint Platform + baseview window handle bridge.
//!
//! Implements `slint::platform::Platform` so Slint components can be created
//! without a native windowing backend. Rendering goes through Slint's
//! `SoftwareRenderer` to a pixel buffer that we blit via wgpu.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

// `HasRawDisplayHandle` / `RwhRawDisplayHandle` are only touched on
// the Linux (X11) arm of `HasRawWindowHandle for ParentWindow`;
// silence the macOS/Windows dead-import warning.
#[allow(unused_imports)]
use raw_window_handle::{
    HasRawDisplayHandle, HasRawWindowHandle, RawDisplayHandle as RwhRawDisplayHandle,
    RawWindowHandle as RwhRawWindowHandle,
};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, PlatformError};
use truce_core::editor::RawWindowHandle;

// ---------------------------------------------------------------------------
// Slint Platform — registered once per process
// ---------------------------------------------------------------------------

// Thread-local slot used to pass a pre-created `MinimalSoftwareWindow` to
// `create_window_adapter()`. Set this before creating a Slint component so
// the component attaches to our window (not a throwaway one).
thread_local! {
    static NEXT_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = RefCell::new(None);
}

struct TrucePlatform;

impl Platform for TrucePlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, PlatformError> {
        // Return the pre-created window if one was set, otherwise create a new one.
        let window = NEXT_WINDOW.with(|slot| slot.borrow_mut().take());
        Ok(window.unwrap_or_else(|| MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer)))
    }
}

thread_local! {
    static PLATFORM_INIT: Cell<bool> = const { Cell::new(false) };
}

/// Ensure the custom Slint platform is registered on the calling thread.
///
/// Slint's `set_platform` is thread-local, so this must be called on every
/// thread that creates Slint components — including the baseview render
/// thread, not just the plugin thread. Idempotent per thread.
pub fn ensure_platform() {
    PLATFORM_INIT.with(|init| {
        if !init.get() {
            let _ = slint::platform::set_platform(Box::new(TrucePlatform));
            init.set(true);
        }
    });
}

/// Create a `MinimalSoftwareWindow` and register it so the next Slint
/// component creation attaches to it. Returns the window for rendering.
///
/// Call this immediately before `MyComponent::new()`.
pub fn create_slint_window() -> Rc<MinimalSoftwareWindow> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    NEXT_WINDOW.with(|slot| *slot.borrow_mut() = Some(window.clone()));
    window
}

// ---------------------------------------------------------------------------
// Pixel buffer rendering helper
// ---------------------------------------------------------------------------

/// Render a `MinimalSoftwareWindow` to an RGBA pixel buffer.
///
/// Uses `PremultipliedRgbaColor` as the native pixel type. The returned
/// buffer is reused across frames, sized to `width * height * 4`.
pub fn render_to_rgba(
    window: &MinimalSoftwareWindow,
    width: u32,
    height: u32,
    px_buf: &mut Vec<PremultipliedRgbaColor>,
    rgba_buf: &mut Vec<u8>,
) {
    let pixel_count = (width * height) as usize;
    px_buf.resize(pixel_count, PremultipliedRgbaColor::default());

    window.draw_if_needed(|renderer| {
        renderer.render(px_buf, width as usize);
    });

    // Copy premultiplied RGBA to byte buffer for wgpu upload.
    rgba_buf.resize(pixel_count * 4, 255);
    for (i, px) in px_buf.iter().enumerate() {
        let off = i * 4;
        rgba_buf[off] = px.red;
        rgba_buf[off + 1] = px.green;
        rgba_buf[off + 2] = px.blue;
        rgba_buf[off + 3] = px.alpha;
    }
}

// ---------------------------------------------------------------------------
// Baseview parent window bridge (shared with egui)
// ---------------------------------------------------------------------------

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
        let ns_view = ns_view_ptr as *mut objc::runtime::Object;
        let window: *mut objc::runtime::Object = msg_send![ns_view, window];
        let scale: f64 = if !window.is_null() {
            msg_send![window, backingScaleFactor]
        } else {
            let screen: *mut objc::runtime::Object = msg_send![objc::class!(NSScreen), mainScreen];
            if !screen.is_null() {
                msg_send![screen, backingScaleFactor]
            } else {
                2.0
            }
        };
        if scale < 1.0 {
            1.0
        } else {
            scale
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn query_backing_scale(_parent: &RawWindowHandle) -> f64 {
    1.0
}

/// Bridge a baseview raw-window-handle 0.5 to a wgpu-compatible surface.
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
