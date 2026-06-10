//! Shared host-side platform helpers for truce GUI backends that
//! embed a wgpu-backed (or CALayer-backed) child view into a
//! DAW-provided parent window.
//!
//! Currently macOS-only: the helpers pin an embedded NSView's top
//! edge to its superview's top edge across host-driven resizes.
//! Linux/Windows hosts manage child-window positioning natively, so
//! these helpers are no-ops there.

#![allow(clippy::module_name_repetitions)]

/// Re-anchor the editor's `NSView` to the **top** of its superview
/// in unflipped Cocoa coordinates.
///
/// The CLAP / LV2 / AU shims set the child's autoresize mask to
/// `NSViewMinYMargin | NSViewMaxXMargin` so the parent-resize
/// cascade keeps the child pinned. AppKit's autoresize math only
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

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsSize {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsRect {
        origin: NsPoint,
        size: NsSize,
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
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsSize {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsRect {
        origin: NsPoint,
        size: NsSize,
    }
    if parent.is_null() {
        return;
    }
    unsafe {
        let parent_obj = parent.cast::<Object>();
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
