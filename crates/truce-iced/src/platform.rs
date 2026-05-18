//! Platform glue for truce-iced - mirrors `truce-egui::platform` and
//! `truce-gpu::platform` in shape.
//!
//! `ParentWindow`, `query_backing_scale`, and `note_linux_scale_factor`
//! are re-exported from `truce-gui` so call sites have one canonical
//! source. The wgpu-surface bridge can't follow the same pattern: iced
//! pulls in `iced_wgpu` 0.13 which depends on **wgpu 0.19**, while
//! `truce-gui` (and `truce-egui` / `truce-gpu`) is on **wgpu 24**.
//! `wgpu::Surface` is therefore a different type in each dep tree, so
//! the canonical helper produces a value `iced_wgpu` can't ingest.
//! `create_wgpu_surface` below is the per-version copy required to
//! bridge baseview's rwh-0.5 handle to the wgpu-0.19 surface type
//! `iced_wgpu` expects. When `iced_wgpu` catches up to wgpu 24, this
//! module collapses to a one-line re-export of the canonical helper.

use iced_wgpu::wgpu;
use raw_window_handle::HasRawWindowHandle;

pub use truce_gui::platform::{ParentWindow, note_linux_scale_factor, query_backing_scale};

/// Bridge a baseview raw-window-handle 0.5 to the wgpu-0.19
/// `SurfaceTargetUnsafe` type that `iced_wgpu` 0.13 expects.
///
/// Logic mirrors `truce_gui::platform::create_wgpu_surface` but
/// targets a different `wgpu::*` namespace (see module doc).
///
/// # Safety
/// The window handle must be valid for the lifetime of the returned
/// surface.
#[must_use]
pub unsafe fn create_wgpu_surface(
    instance: &wgpu::Instance,
    window: &baseview::Window,
) -> Option<wgpu::Surface<'static>> {
    unsafe {
        let rwh = window.raw_window_handle();
        let target = match rwh {
            #[cfg(target_os = "macos")]
            raw_window_handle::RawWindowHandle::AppKit(h) => {
                let ns_view = std::ptr::NonNull::new(h.ns_view)?;
                wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: wgpu::rwh::RawDisplayHandle::AppKit(
                        wgpu::rwh::AppKitDisplayHandle::new(),
                    ),
                    raw_window_handle: wgpu::rwh::RawWindowHandle::AppKit(
                        wgpu::rwh::AppKitWindowHandle::new(ns_view),
                    ),
                }
            }
            #[cfg(target_os = "windows")]
            raw_window_handle::RawWindowHandle::Win32(h) => wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: wgpu::rwh::RawDisplayHandle::Windows(
                    wgpu::rwh::WindowsDisplayHandle::new(),
                ),
                raw_window_handle: wgpu::rwh::RawWindowHandle::Win32(
                    wgpu::rwh::Win32WindowHandle::new(std::num::NonZero::new(h.hwnd as isize)?),
                ),
            },
            #[cfg(target_os = "linux")]
            raw_window_handle::RawWindowHandle::Xlib(h) => {
                use raw_window_handle::HasRawDisplayHandle;
                let raw_window_handle::RawDisplayHandle::Xlib(display) =
                    window.raw_display_handle()
                else {
                    return None;
                };
                let display_ptr = std::ptr::NonNull::new(display.display);
                wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: wgpu::rwh::RawDisplayHandle::Xlib(
                        wgpu::rwh::XlibDisplayHandle::new(display_ptr, display.screen),
                    ),
                    raw_window_handle: wgpu::rwh::RawWindowHandle::Xlib(
                        wgpu::rwh::XlibWindowHandle::new(h.window as std::ffi::c_ulong),
                    ),
                }
            }
            _ => return None,
        };
        instance.create_surface_unsafe(target).ok()
    }
}
