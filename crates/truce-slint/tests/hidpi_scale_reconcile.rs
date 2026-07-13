//! Regression test for issue #201: Slint editors panicking on `HiDPI`.
//!
//! On a `HiDPI` display the editor often opens at scale 1.0 (the pre-window
//! `main_screen_scale()` fallback) and is then corrected to the real scale
//! (e.g. 2.0) by baseview's first `WindowEvent::Resized` - with the *logical*
//! size unchanged. The bug: the `Resized` handler eagerly dispatched
//! `ScaleFactorChanged` to the Slint window (growing its physical extent to
//! 2×) *and* pre-bumped `last_applied_scale`, so both of `on_frame`'s
//! reconcile branches (`pending_size`, keyed on a logical-size change, and
//! `scale.take_change`, keyed on `last_applied_scale`) became no-ops. The
//! cached `last_phys_*` then stayed at the 1× extent while Slint expected the
//! 2× extent, and the software renderer panicked:
//!
//! ```text
//! buffer of size 51040 with 176 pixels per line is too small
//! to handle a window of size 352x580
//! ```
//!
//! (which in turn armed device-loss recovery and cascaded into the wgpu
//! `SwapchainAcquireSemaphore` panic on pump respawn).
//!
//! These tests replay the exact `WindowEvent` sequence each handler variant
//! emits, against the real Slint software renderer, with a real attached
//! component - no GPU, display, or DPI change required. The "buggy" ordering
//! reproduces the panic; the "fixed" ordering (scale reconciled by
//! `on_frame`'s `scale.take_change` branch: dispatch scale + `set_size` +
//! render at the new physical extent) renders cleanly.

use slint::PhysicalSize;
use slint::platform::WindowEvent;
use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor};

use truce_gui::to_physical_px;
use truce_slint::platform;

slint::slint! {
    // A window whose content fills whatever physical extent `set_size`
    // dictates, so the software renderer's buffer-vs-window assertion is
    // exercised against a real `window_item` logical size.
    export component ReproUi inherits Window {
        Rectangle { background: #ff0000; }
    }
}

const LW: u32 = 176;
const LH: u32 = 290;

/// Build a Slint window + attached component the way the editor's `open()`
/// does: size to the open-time physical extent and announce the open-time
/// scale. Returns the window (the component is kept alive by leaking it -
/// these are one-shot test processes).
fn open_at(open_scale: f32) -> std::rc::Rc<MinimalSoftwareWindow> {
    platform::ensure_platform();
    let window = platform::create_slint_window();
    let phys_w = to_physical_px(LW, f64::from(open_scale));
    let phys_h = to_physical_px(LH, f64::from(open_scale));
    window.set_size(slint::WindowSize::Physical(PhysicalSize::new(
        phys_w, phys_h,
    )));
    window.dispatch_event(WindowEvent::ScaleFactorChanged {
        scale_factor: open_scale,
    });
    // Attaches to `window` via the NEXT_WINDOW hand-off in
    // `create_window_adapter`. Leak it so it outlives the test body.
    let ui = ReproUi::new().expect("build ReproUi");
    std::mem::forget(ui);
    window
}

fn render(window: &MinimalSoftwareWindow, phys_w: u32, phys_h: u32) {
    let mut px_buf: Vec<PremultipliedRgbaColor> = Vec::new();
    let mut rgba_buf: Vec<u8> = Vec::new();
    // The live handler forces a redraw each frame before rendering.
    window.request_redraw();
    platform::render_to_rgba(window, phys_w, phys_h, &mut px_buf, &mut rgba_buf);
    // Sanity: the un-premultiplied buffer is sized to the physical extent.
    assert_eq!(rgba_buf.len(), (phys_w * phys_h * 4) as usize);
}

/// The OLD `Resized` handler: dispatch `ScaleFactorChanged` eagerly (grows the
/// Slint window to 2×) and pre-bump `last_applied_scale` so *neither*
/// `on_frame` reconcile branch fires. The cached physical extent stays at 1×,
/// so the render is issued at the 1× extent against a 2× window - the exact
/// mismatch that panics the software renderer.
#[test]
fn buggy_ordering_panics_like_issue_201() {
    let window = open_at(1.0);

    // Resized(scale = 2.0, logical size unchanged): eager scale dispatch.
    window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 2.0 });
    // `last_phys_*` was never updated (both branches skipped), so the render
    // still runs at the open-time 1× extent.
    let stale_phys_w = to_physical_px(LW, 1.0); // 176
    let stale_phys_h = to_physical_px(LH, 1.0); // 290

    // Silence the default panic hook's backtrace for this expected panic.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        render(&window, stale_phys_w, stale_phys_h);
    }));
    std::panic::set_hook(prev);

    let err = result.expect_err("stale 1x render into a 2x window must panic");
    let msg = err
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_default();
    assert!(
        msg.contains("too small"),
        "expected a software-renderer buffer-too-small panic, got: {msg:?}"
    );
}

/// The FIXED handler: the `Resized` handler only records the new scale; the
/// scale change is reconciled by `on_frame`'s `scale.take_change` branch,
/// which dispatches `ScaleFactorChanged`, resizes the Slint window to the new
/// physical extent, and renders there. No panic.
#[test]
fn fixed_ordering_reconciles_and_renders() {
    let window = open_at(1.0);

    // `on_frame` scale-change branch for the 1.0 -> 2.0 transition:
    let new_scale = 2.0f32;
    let phys_w = to_physical_px(LW, f64::from(new_scale)); // 352
    let phys_h = to_physical_px(LH, f64::from(new_scale)); // 580
    window.dispatch_event(WindowEvent::ScaleFactorChanged {
        scale_factor: new_scale,
    });
    window.set_size(slint::WindowSize::Physical(PhysicalSize::new(
        phys_w, phys_h,
    )));

    // Renders at the reconciled 2× extent - must not panic.
    render(&window, phys_w, phys_h);
    assert_eq!((phys_w, phys_h), (352, 580));
}
