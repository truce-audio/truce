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

use std::sync::Arc;
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
                // The Windows narrowing is the lossy edge — `XID` is 32-bit there.
                #[allow(clippy::cast_possible_truncation)]
                {
                    handle.window = window_id as _;
                }
                RwhRawWindowHandle::Xlib(handle)
            }
        }
    }
}

/// Query the backing scale factor from the parent `NSView`'s window.
#[cfg(target_os = "macos")]
#[must_use]
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
        let ns_view = ns_view_ptr.cast::<objc::runtime::Object>();
        let window: *mut objc::runtime::Object = msg_send![ns_view, window];
        let scale: f64 = if window.is_null() {
            let screen: *mut objc::runtime::Object = msg_send![objc::class!(NSScreen), mainScreen];
            if screen.is_null() {
                2.0
            } else {
                msg_send![screen, backingScaleFactor]
            }
        } else {
            msg_send![window, backingScaleFactor]
        };
        if scale < 1.0 { 1.0 } else { scale }
    }
}

#[cfg(target_os = "windows")]
#[must_use]
pub fn query_backing_scale(parent: &RawWindowHandle) -> f64 {
    let hwnd = match parent {
        RawWindowHandle::Win32(ptr) => *ptr,
        _ => return 1.0,
    };
    win32_dpi_scale(hwnd)
}

#[cfg(target_os = "linux")]
#[must_use]
pub fn query_backing_scale(_parent: &RawWindowHandle) -> f64 {
    main_screen_scale()
}

/// Query the main screen's backing scale factor (no parent window needed).
#[cfg(target_os = "macos")]
#[must_use]
pub fn main_screen_scale() -> f64 {
    use objc::{msg_send, sel, sel_impl};
    unsafe {
        let screen: *mut objc::runtime::Object = msg_send![objc::class!(NSScreen), mainScreen];
        if screen.is_null() {
            1.0
        } else {
            let scale: f64 = msg_send![screen, backingScaleFactor];
            if scale < 1.0 { 1.0 } else { scale }
        }
    }
}

#[cfg(target_os = "windows")]
#[must_use]
pub fn main_screen_scale() -> f64 {
    win32_dpi_scale(std::ptr::null_mut())
}

/// Shared, mutable editor scale factor.
///
/// Single source of truth for the live content-scale of an open plugin
/// window. Each GUI backend (egui / iced / slint) constructs one in
/// `Editor::open`, stores it on the editor for `set_scale_factor` to
/// write through, and hands a clone to its baseview `WindowHandler` so
/// the render thread can pick up changes between frames.
///
/// Two writers, one reader-per-frame:
/// - `Editor::set_scale_factor` (host → editor, e.g. CLAP `set_scale`,
///   VST3 Windows `IPlugViewContentScaleSupport`).
/// - `WindowEvent::Resized` (baseview → handler, fired when the OS
///   reports a new content scale, e.g. dragging the window across
///   monitors with different DPIs).
///
/// Most-recent-write wins. The handler tracks a `last_applied_scale`
/// alongside its `EditorScale` clone and, when it observes a divergence
/// at frame start, recomputes physical sizes and reconfigures its
/// surface / renderer.
#[derive(Clone)]
pub struct EditorScale {
    inner: Arc<AtomicU64>,
}

impl EditorScale {
    /// Construct with an initial scale. Non-finite or non-positive
    /// values clamp to 1.0 so callers never have to defend against
    /// `0.0 * size` collapsing the surface.
    #[must_use]
    pub fn new(initial: f64) -> Self {
        let v = if initial.is_finite() && initial > 0.0 {
            initial
        } else {
            1.0
        };
        Self {
            inner: Arc::new(AtomicU64::new(v.to_bits())),
        }
    }

    /// Read the current scale.
    #[must_use]
    pub fn get(&self) -> f64 {
        f64::from_bits(self.inner.load(Ordering::Relaxed))
    }

    /// Read the current scale, narrowed to `f32` for renderer / DSP
    /// use. Display scales never exceed 4.0 in practice, so the f64
    /// → f32 narrowing is invisible.
    #[allow(clippy::cast_possible_truncation)]
    #[must_use]
    pub fn get_f32(&self) -> f32 {
        self.get() as f32
    }

    /// Update the current scale. Non-finite or non-positive values are
    /// silently dropped — callers are forwarding numbers from hosts /
    /// `info.scale()` where a bad value is a host bug, not something
    /// to propagate into the surface config.
    pub fn set(&self, scale: f64) {
        if scale.is_finite() && scale > 0.0 {
            self.inner.store(scale.to_bits(), Ordering::Relaxed);
        } else {
            // Surface the upstream bug at least in debug builds so a
            // host that's emitting bad scales doesn't get silently
            // ignored. Production builds drop quietly to keep the
            // editor running.
            log::warn!(
                "EditorScale::set ignored a bad value ({scale}); \
                 expected finite, positive f64",
            );
        }
    }

    /// Pick up a host-driven scale change since the last frame.
    ///
    /// Reads the current scale (narrowed to `f32`) and compares it
    /// bit-identically against `last`. When the value moved, updates
    /// `last` and returns `Some(cur)`; otherwise returns `None`.
    ///
    /// Used by every editor backend's per-frame loop to gate surface /
    /// renderer reconfiguration on actual host scale events. Bit-equality
    /// is the correct semantics — the cell is written verbatim from
    /// host callbacks, never through accumulating arithmetic, so an
    /// epsilon-based check would either thrash on noise (there is
    /// none) or miss a legitimate `1.0 → 1.0001` host signal.
    #[allow(clippy::cast_possible_truncation, clippy::float_cmp)]
    pub fn take_change(&self, last: &mut f32) -> Option<f32> {
        let cur = self.get() as f32;
        if cur == *last {
            None
        } else {
            *last = cur;
            Some(cur)
        }
    }
}

/// Convert a logical extent (in points) to physical pixels.
///
/// Standardised rounding policy across every truce GUI backend:
/// round to nearest, then clamp the result to `1` so a degenerate
/// `0 × scale` doesn't collapse a wgpu surface (`width: 0` is a
/// validation error). The `logical.max(1)` guard handles the
/// converse — a zero-logical caller can't multiply through to `0`
/// before the round.
///
/// Replaces a mix of truncating `(logical * scale) as u32` casts,
/// `.round() as u32` without a min clamp, and the explicit
/// `.round().max(1.0) as u32` form that landed in
/// `truce-gui::backend_cpu` first. One helper, every site, identical
/// pixel maths.
// Logical pixel sizes are bounded by `u32::MAX / scale`; in practice
// no editor exceeds 16384 logical pixels.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#[inline]
#[must_use]
pub fn to_physical_px(logical: u32, scale: f64) -> u32 {
    (f64::from(logical.max(1)) * scale).round().max(1.0) as u32
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
pub fn note_linux_scale_factor(scale: f64) {
    #[cfg(target_os = "linux")]
    {
        if scale.is_finite() && scale > 0.0 {
            LINUX_SCALE_BITS.store(scale.to_bits(), Ordering::Relaxed);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = scale;
    }
}

#[cfg(target_os = "linux")]
pub fn main_screen_scale() -> f64 {
    // Priority: TRUCE_SCALE env var (dev/test override) → cached scale
    // observed from baseview → 1.0 fallback. No side-channel Xlib calls —
    // those crashed inside NVIDIA's Vulkan driver when invoked from the
    // render thread.
    if let Ok(s) = std::env::var("TRUCE_SCALE")
        && let Ok(v) = s.parse::<f64>()
        && v.is_finite()
        && v > 0.0
    {
        return v;
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

    unsafe extern "system" {
        fn GetDpiForWindow(hwnd: *mut std::ffi::c_void) -> u32;
        fn GetDpiForSystem() -> u32;
    }

    let dpi = if hwnd.is_null() {
        unsafe { GetDpiForSystem() }
    } else {
        let d = unsafe { GetDpiForWindow(hwnd) };
        if d == 0 {
            unsafe { GetDpiForSystem() }
        } else {
            d
        }
    };

    if dpi == 0 {
        1.0
    } else {
        f64::from(dpi) / f64::from(DEFAULT_DPI)
    }
}

/// Bridge a baseview raw-window-handle 0.5 to a wgpu-compatible
/// `SurfaceTargetUnsafe` using rwh 0.6 types.
///
/// # Safety
/// The window handle must be valid for the lifetime of the returned surface.
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
                    wgpu::rwh::Win32WindowHandle::new(std::num::NonZeroIsize::new(hwnd as isize)?),
                );
                let rwh6_display =
                    wgpu::rwh::RawDisplayHandle::Windows(wgpu::rwh::WindowsDisplayHandle::new());
                wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: rwh6_display,
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
                    raw_display_handle: rwh6_display,
                    raw_window_handle: rwh6_window,
                }
            }
            _ => return None,
        };

        instance.create_surface_unsafe(surface_target).ok()
    }
}
