//! Platform window bridging for baseview / wgpu.
//!
//! `create_wgpu_surface` consumes a `baseview::Window`; that
//! window exposes raw-window-handle 0.5 handles but wgpu wants 0.6,
//! so the bridge re-encodes per platform here. Lives in `truce-gpu`
//! so the wgpu pipeline crate is self-contained; the per-OS
//! HWND/NSView lookups + DPI queries live next door in
//! `truce_gui::platform`.

#[cfg(not(target_os = "ios"))]
#[allow(unused_imports)]
use raw_window_handle::{
    HasRawDisplayHandle, HasRawWindowHandle, RawDisplayHandle as RwhRawDisplayHandle,
    RawWindowHandle as RwhRawWindowHandle,
};

#[cfg(target_os = "windows")]
pub(crate) fn current_module_hinstance() -> Option<std::num::NonZeroIsize> {
    unsafe extern "system" {
        fn GetModuleHandleW(lpModuleName: *const u16) -> isize;
    }
    // SAFETY: `GetModuleHandleW(NULL)` is documented to return the running
    // EXE's HMODULE without acquiring a refcount; no threading or aliasing
    // concerns. Returns 0 only in pathological cases (kernel32 missing).
    let hmodule = unsafe { GetModuleHandleW(std::ptr::null()) };
    std::num::NonZeroIsize::new(hmodule)
}

/// wgpu backends for an editor surface embedded in a host-owned
/// child window. Mirror of `truce_gui::platform::editor_wgpu_backends`
/// (truce-gpu can't depend on truce-gui without a dep cycle). Windows
/// is DX12 (the only backend feature compiled in on Windows), macOS
/// is Metal, Linux keeps `PRIMARY`. Keep the two copies in sync.
#[must_use]
pub fn editor_wgpu_backends() -> wgpu::Backends {
    #[cfg(target_os = "windows")]
    {
        wgpu::Backends::DX12
    }
    #[cfg(target_os = "macos")]
    {
        wgpu::Backends::METAL
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        wgpu::Backends::PRIMARY
    }
}

/// `wgpu::InstanceDescriptor` for editor surfaces, pinning the DX12
/// shader compiler to **FXC**. Mirror of
/// `truce_gui::platform::editor_instance_descriptor` - see it for why
/// the wgpu-29 DX12 default (dynamic DXC) breaks inside Pro Tools.
/// Keep the two copies in sync.
#[must_use]
pub fn editor_instance_descriptor() -> wgpu::InstanceDescriptor {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = editor_wgpu_backends();
    desc.backend_options.dx12.shader_compiler = wgpu::Dx12Compiler::Fxc;
    desc
}

/// Bridge a baseview raw-window-handle 0.5 to a wgpu-compatible
/// `SurfaceTargetUnsafe` using rwh 0.6 types.
///
/// # Safety
/// The window handle must be valid for the lifetime of the returned surface.
#[cfg(not(target_os = "ios"))]
#[must_use]
pub unsafe fn create_wgpu_surface(
    instance: &wgpu::Instance,
    window: &baseview::Window,
) -> Option<wgpu::Surface<'static>> {
    unsafe {
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
                    raw_display_handle: Some(rwh6_display),
                    raw_window_handle: rwh6_window,
                }
            }
            #[cfg(target_os = "windows")]
            RwhRawWindowHandle::Win32(handle) => {
                let hwnd = handle.hwnd;
                if hwnd.is_null() {
                    return None;
                }
                let mut win32 =
                    wgpu::rwh::Win32WindowHandle::new(std::num::NonZeroIsize::new(hwnd as isize)?);
                // wgpu's Vulkan backend requires `hinstance` to be set
                // (`vkCreateWin32SurfaceKHR` rejects a null HINSTANCE).
                // baseview leaves the rwh 0.5 `hinstance` field at null,
                // so populate it here with the running module's HMODULE.
                win32.hinstance = current_module_hinstance();
                let rwh6_window = wgpu::rwh::RawWindowHandle::Win32(win32);
                let rwh6_display =
                    wgpu::rwh::RawDisplayHandle::Windows(wgpu::rwh::WindowsDisplayHandle::new());
                wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(rwh6_display),
                    raw_window_handle: rwh6_window,
                }
            }
            #[cfg(target_os = "linux")]
            RwhRawWindowHandle::Xlib(handle) => {
                let RwhRawDisplayHandle::Xlib(display_handle) = window.raw_display_handle() else {
                    return None;
                };
                let display_ptr = std::ptr::NonNull::new(display_handle.display);
                let rwh6_window = wgpu::rwh::RawWindowHandle::Xlib(
                    wgpu::rwh::XlibWindowHandle::new(handle.window),
                );
                let rwh6_display = wgpu::rwh::RawDisplayHandle::Xlib(
                    wgpu::rwh::XlibDisplayHandle::new(display_ptr, display_handle.screen),
                );
                wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(rwh6_display),
                    raw_window_handle: rwh6_window,
                }
            }
            _ => return None,
        };

        instance.create_surface_unsafe(surface_target).ok()
    }
}
