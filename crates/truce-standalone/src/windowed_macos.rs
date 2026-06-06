//! macOS helpers for the standalone window's resize affordance.
//!
//! `baseview-truce` 0.1.1-truce.6 creates its `NSWindow` with the
//! `Titled | Closable | Miniaturizable` style mask only; without
//! `Resizable` the window has no drag-the-edge affordance and
//! `AppKit` refuses drag-resize attempts. When the plugin's editor
//! opts into resizing we OR the bit in here, after baseview has
//! finished its own window setup. baseview is unchanged.

use objc::runtime::Object;
use objc::{msg_send, sel, sel_impl};

/// `AppKit`'s `NSWindowStyleMaskResizable` value. Lives in
/// `<AppKit/NSWindow.h>` as `NSWindowStyleMaskResizable = 1 << 3`.
const NS_WINDOW_STYLE_MASK_RESIZABLE: u64 = 1 << 3;

/// Add the `Resizable` bit to the standalone's `NSWindow`.
///
/// `ns_window` is the raw `NSWindow *` baseview populates on
/// `RawWindowHandle::AppKit::ns_window` for its parentless (i.e.
/// standalone) windows. We deliberately avoid `[ns_view window]`
/// here because baseview calls `setContentView:` *after* the
/// `Window::open_blocking` build closure runs, so the view's
/// window association is nil at the moment standalone wants to
/// adjust the style mask.
///
/// # Safety
///
/// Must run on the main thread and only after baseview has
/// finished its `NSWindow` initialisation. The caller is
/// responsible for ensuring `ns_window` is a live Objective-C
/// pointer.
pub unsafe fn make_resizable(ns_window: *mut std::ffi::c_void) {
    if ns_window.is_null() {
        return;
    }
    let window = ns_window.cast::<Object>();
    let mask: u64 = unsafe { msg_send![window, styleMask] };
    let new_mask = mask | NS_WINDOW_STYLE_MASK_RESIZABLE;
    let _: () = unsafe { msg_send![window, setStyleMask: new_mask] };
}

/// `CGFloat` is `f64` on 64-bit Apple platforms (which is everything
/// truce targets - `aarch64` and `x86_64`). Mirror the layout
/// `AppKit` uses for `NSRect` / `NSSize` instead of pulling in a
/// dependency.
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

/// Read the standalone `NSWindow`'s content frame size in logical
/// points. baseview-truce 0.1.1-truce.6 only fires `Resized` for
/// `viewDidChangeBackingProperties` (DPI changes); user-driven OS
/// window drags never reach the `WindowHandler`. Polling
/// `[ns_window contentLayoutRect]` from `on_frame` lets the outer
/// `StandaloneHandler` detect those drags and forward to
/// `editor.set_size`. Returns `None` if the window pointer is null
/// or the call fails to produce a usable size.
///
/// # Safety
///
/// Must run on the main thread. The caller is responsible for
/// ensuring `ns_window` is a live Objective-C pointer.
pub unsafe fn content_logical_size(ns_window: *mut std::ffi::c_void) -> Option<(u32, u32)> {
    if ns_window.is_null() {
        return None;
    }
    let window = ns_window.cast::<Object>();
    // `contentLayoutRect` returns the rect inside the window's
    // content view, excluding title bar / toolbar - matches what
    // the editor occupies. Native `NSRect` return; objc 0.2's
    // `msg_send!` infers the layout from the bound type.
    let rect: NsRect = unsafe { msg_send![window, contentLayoutRect] };
    let w = rect.size.width.round();
    let h = rect.size.height.round();
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((w as u32, h as u32))
}
