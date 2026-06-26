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

/// Post-open present settle for wgpu editor backends on Windows.
///
/// A freshly-opened `WS_CHILD` editor window isn't composited by DWM for
/// a brief moment after it becomes visible. The child flip-swapchain only
/// supports `Fifo`, so `PresentMode::AutoNoVsync` falls back to it, and a
/// `present()` to the uncomposited window blocks the host's GUI thread
/// until composition exists - a focus change (alt-tab) forces it,
/// otherwise the DAW appears frozen on editor open. Non-blocking present
/// modes aren't available for these child swapchains, so the fix is to
/// not present until the window has had a moment to be composited.
///
/// A backend holds one of these and, after its visibility check, skips
/// rendering while [`ready`](Self::ready) returns `false`. Only the first
/// ~quarter second after a fresh open is held off; steady-state frames are
/// unaffected. No-op (always ready) on every platform but Windows.
#[derive(Default)]
pub struct PresentSettle {
    /// When the window first became presentable. `None` until the first
    /// `ready()` call that observes a visible window.
    #[cfg(target_os = "windows")]
    since: Option<std::time::Instant>,
}

impl PresentSettle {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether enough time has passed since the window first became
    /// presentable for its first present to be safe. Call this *after*
    /// the occlusion/visibility check so the clock starts when the
    /// window is actually shown. Always `true` off Windows.
    #[must_use]
    pub fn ready(&mut self) -> bool {
        #[cfg(target_os = "windows")]
        {
            const SETTLE: std::time::Duration = std::time::Duration::from_millis(250);
            self.since
                .get_or_insert_with(std::time::Instant::now)
                .elapsed()
                >= SETTLE
        }
        #[cfg(not(target_os = "windows"))]
        {
            true
        }
    }
}
