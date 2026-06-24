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
/// macOS-only; always `false` elsewhere - Linux/Windows hosts manage
/// visibility natively and don't exhibit the pile-up.
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

#[cfg(not(target_os = "macos"))]
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
