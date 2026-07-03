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

/// Pin the standalone window to a fixed content size so a non-resizable
/// editor's window can't be grown. baseview creates the window without
/// `Resizable` (no edge-drag), but the green zoom button /
/// double-click-titlebar `zoom:` and programmatic resizes can still
/// enlarge it - and the fixed-size editor child doesn't follow, leaving
/// an unpainted (white) margin. Setting content min == max == the
/// editor's size clamps every resize path (drag, zoom, scripted) to the
/// editor's geometry. macOS equivalent of `windowed_x11::pin_size`
/// and `windowed_windows::lock_window`.
///
/// # Safety
///
/// Must run on the main thread and only after baseview has finished its
/// `NSWindow` initialisation. The caller is responsible for ensuring
/// `ns_window` is a live Objective-C pointer.
pub unsafe fn pin_content_size(ns_window: *mut std::ffi::c_void, w: u32, h: u32) {
    if ns_window.is_null() {
        return;
    }
    let window = ns_window.cast::<Object>();
    let size = NsSize {
        width: f64::from(w),
        height: f64::from(h),
    };
    let _: () = unsafe { msg_send![window, setContentMinSize: size] };
    let _: () = unsafe { msg_send![window, setContentMaxSize: size] };
}

/// Enforce the editor's `min_size` / `max_size` / `aspect_ratio` on the
/// resizable standalone `NSWindow` through `AppKit`'s native constraint
/// properties: `contentMinSize` / `contentMaxSize` clamp edge-drags and
/// the zoom button's standard frame, and `contentAspectRatio` keeps
/// interactive drags on ratio. All content-space logical points -
/// matching the editor's units; `AppKit` accounts for the title bar
/// itself. macOS counterpart of
/// `windowed_windows::install_size_limits`.
///
/// Programmatic `setFrame:` bypasses `AppKit`'s min/max (the hole
/// `WM_GETMINMAXINFO` closes on Windows), but the standalone's own
/// programmatic resizes already route through the editor's clamped
/// `request_resize` path, so drags and zoom are the paths that matter
/// here.
///
/// # Safety
///
/// Must run on the main thread and only after baseview has finished
/// its `NSWindow` initialisation. The caller is responsible for
/// ensuring `ns_window` is a live Objective-C pointer.
pub unsafe fn install_content_limits(
    ns_window: *mut std::ffi::c_void,
    min: (u32, u32),
    max: (u32, u32),
    aspect: Option<(u32, u32)>,
) {
    if ns_window.is_null() {
        return;
    }
    let window = ns_window.cast::<Object>();
    let min_size = NsSize {
        width: f64::from(min.0.max(1)),
        height: f64::from(min.1.max(1)),
    };
    let _: () = unsafe { msg_send![window, setContentMinSize: min_size] };
    // `u32::MAX` (the trait's "unbounded") maps to a max far beyond any
    // display; AppKit treats it the same as its own FLT_MAX default.
    let max_size = NsSize {
        width: f64::from(max.0),
        height: f64::from(max.1),
    };
    let _: () = unsafe { msg_send![window, setContentMaxSize: max_size] };
    if let Some((num, denom)) = aspect
        && num > 0
        && denom > 0
    {
        let ratio = NsSize {
            width: f64::from(num),
            height: f64::from(denom),
        };
        let _: () = unsafe { msg_send![window, setContentAspectRatio: ratio] };
    }
}

/// Disable maximize (zoom) and native fullscreen on the standalone
/// `NSWindow` while keeping it edge-drag resizable.
///
/// Two things make a Mac window "maximize": the green title-bar
/// button, which on modern macOS toggles native fullscreen (Option-
/// click zooms instead), and the window's collection behaviour, which
/// gates the fullscreen path and the View ▸ Enter Full Screen menu /
/// `⌃⌘F`. We disable the zoom button (`setEnabled:NO` on
/// `NSWindowZoomButton`, which also kills the double-click-titlebar
/// zoom) and switch the collection behaviour to `FullScreenNone`, so
/// neither the button nor the shortcut can grow the window past the
/// editor's `max_size` and leave an unpainted margin. Edge-drag resize
/// (the `Resizable` mask `make_resizable` added) is untouched.
///
/// Call after [`make_resizable`]; both target the same `NSWindow`.
/// Linux equivalent: `windowed_x11::disable_maximize`; Windows:
/// `windowed_windows::disable_maximize`.
///
/// # Safety
///
/// Must run on the main thread and only after baseview has finished
/// its `NSWindow` initialisation. The caller is responsible for
/// ensuring `ns_window` is a live Objective-C pointer.
pub unsafe fn disable_zoom(ns_window: *mut std::ffi::c_void) {
    // `NSWindowButton` selector value for the green zoom button
    // (`NSWindowZoomButton = 2`).
    const NS_WINDOW_ZOOM_BUTTON: u64 = 2;
    // `NSWindowCollectionBehavior` bits: `FullScreenPrimary = 1 << 7`
    // and `FullScreenAuxiliary = 1 << 8` are the two that opt a window
    // into native fullscreen; `FullScreenNone = 1 << 9` forbids it.
    const NS_FULLSCREEN_PRIMARY: u64 = 1 << 7;
    const NS_FULLSCREEN_AUXILIARY: u64 = 1 << 8;
    const NS_FULLSCREEN_NONE: u64 = 1 << 9;
    if ns_window.is_null() {
        return;
    }
    let window = ns_window.cast::<Object>();

    // Grey out + disable the green button (covers click and the
    // title-bar double-click zoom gesture). The button can be nil on
    // a borderless window; baseview's is `Titled`, so it exists, but
    // guard anyway.
    let zoom_button: *mut Object =
        unsafe { msg_send![window, standardWindowButton: NS_WINDOW_ZOOM_BUTTON] };
    if !zoom_button.is_null() {
        let _: () = unsafe { msg_send![zoom_button, setEnabled: false] };
    }

    // Forbid native fullscreen so `⌃⌘F` / the View menu can't bypass
    // the disabled button. Clear any fullscreen-enabling bits first,
    // then set `FullScreenNone`.
    let behavior: u64 = unsafe { msg_send![window, collectionBehavior] };
    let new_behavior =
        (behavior & !(NS_FULLSCREEN_PRIMARY | NS_FULLSCREEN_AUXILIARY)) | NS_FULLSCREEN_NONE;
    let _: () = unsafe { msg_send![window, setCollectionBehavior: new_behavior] };
}

/// Give the editor's embedded child view a flexible margin on every
/// side so `AppKit` keeps it **centred** (at a fixed size) as the
/// standalone window grows, instead of stretching it to fill.
///
/// Pairs with [`layout_child_centered`]: this sets the autoresizing
/// behaviour once at open so the child stays centred smoothly during a
/// live edge-drag between our per-frame layouts; `layout_child_centered`
/// then updates the child's actual *size* (and re-centres) whenever the
/// editor clamps or follows a resize. The old behaviour
/// (`NSViewWidthSizable | NSViewHeightSizable`) stretched the child to
/// fill, so a clamped editor (bounded `max_size`) left its surface in
/// the top-left with the grown area beside it; centring distributes
/// that freed space evenly, matching the Linux `center_child` path.
///
/// # Safety
///
/// Must run on the main thread and only after baseview has finished
/// adding its child view to the `NSWindow`'s content view. The
/// caller is responsible for ensuring `ns_view` is a live
/// Objective-C pointer.
pub unsafe fn install_subview_centering(ns_view: *mut std::ffi::c_void) {
    // Cocoa autoresizing-mask flags for fixed-size centring: a
    // flexible margin on each side (`NSViewMinXMargin = 1`,
    // `MaxXMargin = 4`, `MinYMargin = 8`, `MaxYMargin = 32`) lets
    // AppKit split the superview's growth across all four margins,
    // holding the view centred without resizing it.
    const NSVIEW_MIN_X_MARGIN: u64 = 1;
    const NSVIEW_MAX_X_MARGIN: u64 = 4;
    const NSVIEW_MIN_Y_MARGIN: u64 = 8;
    const NSVIEW_MAX_Y_MARGIN: u64 = 32;
    if ns_view.is_null() {
        return;
    }
    // The caller hands us baseview's standalone `NSView` (not the
    // `NSWindow`). `Window::open_blocking` sets baseview's view as
    // the `NSWindow.contentView` *after* the build closure returns
    // - while we're inside the build closure, the `NSWindow`'s
    // contentView is still its default vanilla view, so walking
    // `[ns_window contentView].subviews` finds nothing. baseview's
    // own view, however, is already the parent of the editor's
    // child by the time `editor.open()` returns (baseview's
    // `open_parented` calls `parent_view.addSubview(&new_ns_view)`
    // synchronously). Walking *that* view's subviews finds the
    // editor's NSView reliably.
    let parent = ns_view.cast::<Object>();
    let subviews: *mut Object = unsafe { msg_send![parent, subviews] };
    if subviews.is_null() {
        return;
    }
    let count: usize = unsafe { msg_send![subviews, count] };
    let mask =
        NSVIEW_MIN_X_MARGIN | NSVIEW_MAX_X_MARGIN | NSVIEW_MIN_Y_MARGIN | NSVIEW_MAX_Y_MARGIN;
    for i in 0..count {
        let child: *mut Object = unsafe { msg_send![subviews, objectAtIndex: i] };
        if child.is_null() {
            continue;
        }
        let _: () = unsafe { msg_send![child, setAutoresizingMask: mask] };
    }
}

/// Size the editor's child view to `(child_w, child_h)` logical points
/// and centre it within an `(outer_w, outer_h)` content area. Called
/// each frame from the macOS resize poll so the child tracks the
/// editor's clamped/followed size and sits centred when the window is
/// larger than it. No-op when the child is already at that frame, so
/// per-frame calls don't thrash `AppKit`.
///
/// Origin is clamped non-negative: a child larger than the content
/// area pins to the bottom-left rather than going off-screen (the
/// equivalent of Linux `center_child`'s top-left clamp; `AppKit`'s
/// y-axis is bottom-up, but the margins are symmetric so it reads the
/// same).
///
/// # Safety
///
/// Must run on the main thread; `ns_view` must be the live baseview
/// standalone `NSView` whose first subview is the editor's child.
pub unsafe fn layout_child_centered(
    ns_view: *mut std::ffi::c_void,
    child_w: u32,
    child_h: u32,
    outer_w: u32,
    outer_h: u32,
) {
    if ns_view.is_null() {
        return;
    }
    let parent = ns_view.cast::<Object>();
    let subviews: *mut Object = unsafe { msg_send![parent, subviews] };
    if subviews.is_null() {
        return;
    }
    let count: usize = unsafe { msg_send![subviews, count] };
    if count == 0 {
        return;
    }
    let child: *mut Object = unsafe { msg_send![subviews, objectAtIndex: 0usize] };
    if child.is_null() {
        return;
    }

    let cw = f64::from(child_w);
    let ch = f64::from(child_h);
    let x = ((f64::from(outer_w) - cw) / 2.0).max(0.0);
    let y = ((f64::from(outer_h) - ch) / 2.0).max(0.0);

    let current: NsRect = unsafe { msg_send![child, frame] };
    // Exact compare is fine: both sides are whole-point values we set /
    // AppKit echoes back, never accumulated arithmetic.
    #[allow(clippy::float_cmp)]
    if current.origin.x == x
        && current.origin.y == y
        && current.size.width == cw
        && current.size.height == ch
    {
        return;
    }
    let frame = NsRect {
        origin: NsPoint { x, y },
        size: NsSize {
            width: cw,
            height: ch,
        },
    };
    let _: () = unsafe { msg_send![child, setFrame: frame] };
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
