//! Shared host-side platform helpers for truce GUI backends that
//! embed a wgpu-backed (or CALayer-backed) child view into a
//! DAW-provided parent window.
//!
//! Currently macOS-only: the helpers pin an embedded `NSView`'s top
//! edge to its superview's top edge across host-driven resizes.
//! Linux/Windows hosts manage child-window positioning natively, so
//! these helpers are no-ops there.

#![allow(clippy::module_name_repetitions)]

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct NsPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct NsSize {
    width: f64,
    height: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct NsRect {
    origin: NsPoint,
    size: NsSize,
}

/// Re-anchor the editor's `NSView` to the **top** of its superview
/// in unflipped Cocoa coordinates.
///
/// The CLAP / LV2 / AU shims set the child's autoresize mask to
/// `NSViewMinYMargin | NSViewMaxXMargin` so the parent-resize
/// cascade keeps the child pinned. `AppKit`'s autoresize math only
/// runs when the *parent* resizes, though - resizing the *child*
/// (via `baseview::Window::resize` / `setFrameSize:`) leaves the
/// origin alone, which silently drifts the child off-anchor: a
/// taller child grows *down* from its existing origin instead of
/// staying anchored to the parent's top. Visually that looks like
/// the editor's header / first row disappearing above the visible
/// plug-in area.
///
/// Call this on macOS each frame (e.g. from `WindowHandler::on_frame`)
/// so the child's origin tracks its size. No-op on non-macOS.
#[cfg(target_os = "macos")]
pub fn reanchor_to_superview_top(handle: raw_window_handle::RawWindowHandle) {
    use objc::{msg_send, sel, sel_impl};

    let view_ptr = match handle {
        raw_window_handle::RawWindowHandle::AppKit(h) => h.ns_view,
        _ => return,
    };
    if view_ptr.is_null() {
        return;
    }

    unsafe {
        let view = view_ptr.cast::<objc::runtime::Object>();
        let superview: *mut objc::runtime::Object = msg_send![view, superview];
        if superview.is_null() {
            return;
        }
        let parent_frame: NsRect = msg_send![superview, frame];
        let child_frame: NsRect = msg_send![view, frame];
        let new_y = parent_frame.size.height - child_frame.size.height;
        if (new_y - child_frame.origin.y).abs() < f64::EPSILON {
            return;
        }
        let new_origin = NsPoint {
            x: child_frame.origin.x,
            y: new_y,
        };
        let _: () = msg_send![view, setFrameOrigin: new_origin];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn reanchor_to_superview_top(_handle: raw_window_handle::RawWindowHandle) {}

/// Whether a GUI backend's per-frame `on_frame` should skip all work
/// this tick.
///
/// Returns `true` when the editor's `NSView` is detached from any
/// window - the editor was torn down but baseview's frame timer is
/// still firing (notably AU, which may not call `gui_close`) - or
/// when the host window is not visible (minimized or fully occluded).
///
/// Skipping occluded frames is the load-bearing part: a non-visible
/// window can't present, so any frame a backend renders queues a GPU
/// drawable that can't be drained, and they pile up unbounded (tens of
/// GB of wired memory) until the window returns to front. The
/// `NSWindowOcclusionStateVisible` bit is the authoritative early
/// signal, so this must be called first thing in `on_frame`.
///
/// On macOS the `NSWindowOcclusionStateVisible` bit is the
/// authoritative signal; on Windows we skip when the host's child
/// window is hidden or minimized (`IsWindowVisible` / `IsIconic`).
/// Always `false` on Linux, which manages visibility natively and
/// doesn't exhibit the pile-up.
///
/// The Windows case matters for a different reason than macOS: an
/// embedded editor is a `WS_CHILD` of the host window, so its
/// `on_frame` runs on the host's GUI thread. Rendering + a blocking
/// `present` to a window the user can't see burns that thread for
/// nothing and can back up the swapchain; skipping keeps the host
/// (REAPER, etc.) responsive while its FX window is closed.
#[cfg(target_os = "macos")]
#[must_use]
pub fn should_skip_frame(handle: raw_window_handle::RawWindowHandle) -> bool {
    use objc::{msg_send, sel, sel_impl};

    let view_ptr = match handle {
        raw_window_handle::RawWindowHandle::AppKit(h) => h.ns_view,
        _ => return false,
    };
    if view_ptr.is_null() {
        return true;
    }

    unsafe {
        let view = view_ptr.cast::<objc::runtime::Object>();
        let window: *mut objc::runtime::Object = msg_send![view, window];
        if window.is_null() {
            // Detached from any window - nothing to present into.
            return true;
        }
        // `NSWindowOcclusionStateVisible` == 1 << 1. Bit clear => the
        // window is not visible (minimized or fully covered).
        let state: u64 = msg_send![window, occlusionState];
        state & (1 << 1) == 0
    }
}

#[cfg(target_os = "windows")]
#[must_use]
pub fn should_skip_frame(handle: raw_window_handle::RawWindowHandle) -> bool {
    unsafe extern "system" {
        fn IsWindowVisible(hwnd: *mut std::ffi::c_void) -> i32;
        fn IsIconic(hwnd: *mut std::ffi::c_void) -> i32;
    }

    let hwnd = match handle {
        raw_window_handle::RawWindowHandle::Win32(h) => h.hwnd,
        _ => return false,
    };
    if hwnd.is_null() {
        return true;
    }
    // SAFETY: both are pure state queries on a window handle baseview
    // owns for the editor's lifetime; no aliasing or threading concerns,
    // and they're called from the GUI thread that owns the HWND.
    unsafe { IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[must_use]
pub fn should_skip_frame(_handle: raw_window_handle::RawWindowHandle) -> bool {
    false
}

/// Walk every direct subview of the host-provided parent `NSView`
/// and pin its top edge to the parent's top in unflipped Cocoa
/// coordinates. Used by GUI backends that don't expose their own
/// child `Window` per-frame (vizia) - they hand us the parent
/// handle they got at `Editor::open` time and we walk the subview
/// tree the host installed our backend's view into.
#[cfg(target_os = "macos")]
pub fn reanchor_all_children_to_top(parent: *mut std::ffi::c_void) {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};

    if parent.is_null() {
        return;
    }
    unsafe {
        let parent_obj = parent.cast::<Object>();
        // Skip a parent that's been detached from its window: a sign
        // the host is tearing the editor down, after which walking its
        // subviews risks messaging a freed view. Mirrors the liveness
        // guard in `should_skip_frame`.
        let window: *mut Object = msg_send![parent_obj, window];
        if window.is_null() {
            return;
        }
        let parent_frame: NsRect = msg_send![parent_obj, frame];
        let subviews: *mut Object = msg_send![parent_obj, subviews];
        if subviews.is_null() {
            return;
        }
        let count: usize = msg_send![subviews, count];
        for i in 0..count {
            let child: *mut Object = msg_send![subviews, objectAtIndex: i];
            if child.is_null() {
                continue;
            }
            let child_frame: NsRect = msg_send![child, frame];
            let new_y = parent_frame.size.height - child_frame.size.height;
            if (new_y - child_frame.origin.y).abs() < f64::EPSILON {
                continue;
            }
            let new_origin = NsPoint {
                x: child_frame.origin.x,
                y: new_y,
            };
            let _: () = msg_send![child, setFrameOrigin: new_origin];
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn reanchor_all_children_to_top(_parent: *mut std::ffi::c_void) {}

/// Whether hardware-accelerated OpenGL (the WGL extension entry
/// points a modern GL context needs) is available in this process.
///
/// When the GPU driver's OpenGL ICD is absent or broken (mismatched
/// driver packages, disabled adapter, RDP session), `wglCreateContext`
/// silently hands back Microsoft's software "GDI Generic" GL 1.1
/// context, which exposes **no** WGL extensions. GL-based editors
/// (vizia via Skia) then die inside window creation - under the Win32
/// window proc, where a panic cannot unwind and aborts the entire
/// host. Callers probe this *before* opening such an editor and skip
/// the open instead.
///
/// The probe mirrors baseview's own bootstrap: a hidden throwaway
/// window, a basic pixel format, a legacy `wglCreateContext`, then a
/// `wglGetProcAddress` lookup of the two extensions context creation
/// requires. Everything is torn down before returning.
#[cfg(target_os = "windows")]
#[must_use]
pub fn wgl_extensions_available() -> bool {
    use windows_sys::Win32::Graphics::Gdi::{GetDC, ReleaseDC};
    use windows_sys::Win32::Graphics::OpenGL::{
        ChoosePixelFormat, SetPixelFormat, wglCreateContext, wglDeleteContext, wglGetProcAddress,
        wglMakeCurrent, PFD_DRAW_TO_WINDOW, PFD_MAIN_PLANE, PFD_SUPPORT_OPENGL, PFD_TYPE_RGBA,
        PIXELFORMATDESCRIPTOR,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, UnregisterClassW,
        CS_OWNDC, WNDCLASSW,
    };
    use windows_sys::core::s;

    // UTF-16, NUL-terminated: "truce-wgl-probe".
    const CLASS_NAME: &[u16] = &[
        0x74, 0x72, 0x75, 0x63, 0x65, 0x2d, 0x77, 0x67, 0x6c, 0x2d, 0x70, 0x72, 0x6f, 0x62, 0x65,
        0,
    ];

    unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let class = WNDCLASSW {
            style: CS_OWNDC,
            lpfnWndProc: Some(DefWindowProcW),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: CLASS_NAME.as_ptr(),
        };
        let atom = RegisterClassW(&class);
        if atom == 0 {
            return false;
        }
        let hwnd = CreateWindowExW(
            0,
            CLASS_NAME.as_ptr(),
            CLASS_NAME.as_ptr(),
            0,
            0,
            0,
            1,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            UnregisterClassW(CLASS_NAME.as_ptr(), hinstance);
            return false;
        }

        let mut available = false;
        let hdc = GetDC(hwnd);
        if !hdc.is_null() {
            let pfd = PIXELFORMATDESCRIPTOR {
                nSize: size_of::<PIXELFORMATDESCRIPTOR>() as u16,
                nVersion: 1,
                dwFlags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL,
                iPixelType: PFD_TYPE_RGBA as u8,
                cColorBits: 32,
                cDepthBits: 24,
                cStencilBits: 8,
                iLayerType: PFD_MAIN_PLANE as u8,
                ..std::mem::zeroed()
            };
            let format = ChoosePixelFormat(hdc, &pfd);
            if format != 0 && SetPixelFormat(hdc, format, &pfd) != 0 {
                let hglrc = wglCreateContext(hdc);
                if !hglrc.is_null() {
                    wglMakeCurrent(hdc, hglrc);
                    available = wglGetProcAddress(s!("wglChoosePixelFormatARB")).is_some()
                        && wglGetProcAddress(s!("wglCreateContextAttribsARB")).is_some();
                    wglMakeCurrent(hdc, std::ptr::null_mut());
                    wglDeleteContext(hglrc);
                }
            }
            ReleaseDC(hwnd, hdc);
        }
        DestroyWindow(hwnd);
        UnregisterClassW(CLASS_NAME.as_ptr(), hinstance);
        available
    }
}
