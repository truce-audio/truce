//! Platform bridging: custom Slint Platform + baseview window handle bridge.
//!
//! Implements `slint::platform::Platform` so Slint components can be created
//! without a native windowing backend. Rendering goes through Slint's
//! `SoftwareRenderer` to a pixel buffer that we blit via wgpu.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, PlatformError};

// Baseview parent-window bridge, wgpu surface constructor, and
// per-OS scale-factor query are shared with the other GUI editor
// crates — re-exported from `truce_gui::platform` so this module
// only carries Slint-specific platform glue.
pub use truce_gui::platform::{ParentWindow, create_wgpu_surface, query_backing_scale};

// ---------------------------------------------------------------------------
// Slint Platform — registered once per process
// ---------------------------------------------------------------------------

// Thread-local slot used to pass a pre-created `MinimalSoftwareWindow` to
// `create_window_adapter()`. Set this before creating a Slint component so
// the component attaches to our window (not a throwaway one).
thread_local! {
    static NEXT_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

struct TrucePlatform;

impl Platform for TrucePlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, PlatformError> {
        // Return the pre-created window if one was set, otherwise create a new one.
        let window = NEXT_WINDOW.with(|slot| slot.borrow_mut().take());
        Ok(window.unwrap_or_else(|| MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer)))
    }
}

thread_local! {
    /// Per-thread outcome of the one-time `set_platform` attempt.
    ///
    /// `None` = haven't attempted yet, `Some(Ok(()))` = our `TrucePlatform`
    /// is the active platform on this thread (so the `NEXT_WINDOW`
    /// hand-off in `create_window_adapter` will fire), `Some(Err(()))` =
    /// another platform was already registered (Slint's `set_platform`
    /// is itself a `OnceCell` and refuses to be replaced) and our
    /// `create_window_adapter` override will never run on this thread.
    static PLATFORM_STATE: Cell<Option<Result<(), ()>>> = const { Cell::new(None) };
}

/// Ensure the custom Slint platform is registered on the calling thread.
///
/// Slint's `set_platform` is thread-local *and* one-shot: the first
/// successful call wins; subsequent calls — by us or anyone else
/// linking against the same `slint` runtime — return `Err`. We can't
/// recover from that, so the goal here is to record what actually
/// happened the first time `ensure_platform` ran on this thread and
/// stop trying. The retained `Some(Ok)` / `Some(Err)` distinction is
/// what lets diagnostic code tell "we own the platform" from "we're a
/// guest on someone else's", instead of the previous bool that flipped
/// to `true` regardless of outcome.
///
/// Must be called on every thread that creates Slint components —
/// including the baseview render thread, not just the plugin thread.
/// Idempotent per thread.
pub fn ensure_platform() {
    PLATFORM_STATE.with(|state| {
        if state.get().is_some() {
            return;
        }
        match slint::platform::set_platform(Box::new(TrucePlatform)) {
            Ok(()) => state.set(Some(Ok(()))),
            Err(_) => {
                state.set(Some(Err(())));
                log::warn!(
                    "[truce-slint] slint::platform::set_platform returned Err — \
                     another platform is already registered on this thread; the \
                     pre-attached MinimalSoftwareWindow handed off via NEXT_WINDOW \
                     won't be picked up by Component::new(), so the editor will \
                     render to a different window than the one we hold for blitting"
                );
            }
        }
    });
}

/// Create a `MinimalSoftwareWindow` and register it so the next Slint
/// component creation attaches to it. Returns the window for rendering.
///
/// Call this immediately before `MyComponent::new()`. Panics if
/// `ensure_platform` failed on this thread (typically because some
/// other crate or earlier `slint::platform::set_platform` won the
/// per-thread one-shot first); in that scenario the
/// `NEXT_WINDOW` hand-off would silently *not* be picked up by the
/// active platform's `create_window_adapter`, and we'd end up
/// rendering to a different window than the one the handler holds for
/// blitting. The panic surfaces the failure at editor open time
/// instead of producing a black or stale frame at runtime.
pub fn create_slint_window() -> Rc<MinimalSoftwareWindow> {
    PLATFORM_STATE.with(|state| match state.get() {
        Some(Ok(())) => {}
        Some(Err(())) => panic!(
            "[truce-slint] cannot create a Slint window — `ensure_platform` \
             returned Err on this thread (another platform was registered \
             first). The pre-attached MinimalSoftwareWindow handed off via \
             NEXT_WINDOW won't be picked up by Component::new(), so the \
             editor would render to the wrong window."
        ),
        None => panic!(
            "[truce-slint] cannot create a Slint window — `ensure_platform` \
             has not been called on this thread. Call it before creating \
             Slint components."
        ),
    });
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    NEXT_WINDOW.with(|slot| *slot.borrow_mut() = Some(window.clone()));
    window
}

// ---------------------------------------------------------------------------
// Pixel buffer rendering helper
// ---------------------------------------------------------------------------

/// Per-α reciprocal table: `UNPREMUL_LUT[a] = floor((255 << 16) / a)`
/// for `a ∈ 1..=255`, slot `0` unused (the `α == 0` fast path skips
/// the lookup). Lets us replace the per-channel divide in
/// `render_to_rgba` with a multiply + shift, which is ~5-10× faster
/// on x86 (a 32-bit divide is ~20 cycles vs ~3 for a multiply).
///
/// Bit-for-bit-equivalent to the previous `(c * 255) / a` formulation
/// in normal use: for `c ≤ a` (the premultiplied-alpha invariant),
/// `floor(c * floor(255 << 16 / a) / 65536) == floor(c * 255 / a)`,
/// since the LUT-side floor's drop is at most `(a-1) * c / (a * 65536)`
/// < `c / 65536` ≤ `255/65536 ≈ 0.004`, never enough to cross an
/// integer boundary in the result.
const UNPREMUL_LUT: [u32; 256] = build_unpremul_lut();

const fn build_unpremul_lut() -> [u32; 256] {
    let mut lut = [0u32; 256];
    let mut a: u32 = 1;
    while a <= 255 {
        lut[a as usize] = (255u32 << 16) / a;
        a += 1;
    }
    lut
}

/// Render a `MinimalSoftwareWindow` to an RGBA pixel buffer.
///
/// Uses `PremultipliedRgbaColor` as the native pixel type. The returned
/// buffer is reused across frames, sized to `width * height * 4`.
pub fn render_to_rgba(
    window: &MinimalSoftwareWindow,
    width: u32,
    height: u32,
    px_buf: &mut Vec<PremultipliedRgbaColor>,
    rgba_buf: &mut Vec<u8>,
) {
    let pixel_count = (width * height) as usize;
    px_buf.resize(pixel_count, PremultipliedRgbaColor::default());

    window.draw_if_needed(|renderer| {
        renderer.render(px_buf, width as usize);
    });

    // Un-premultiply Slint's premultiplied output before uploading.
    // The blit shader re-premultiplies in linear space, which is what
    // an `Rgba8UnormSrgb` texture + sRGB surface needs to draw
    // antialiased edges and translucent overlays at the correct
    // brightness. Writing premultiplied sRGB bytes directly here
    // (the previous behavior) made every alpha < 1 pixel render too
    // dark on screen — Slint multiplies in sRGB byte space, so the
    // linear sample comes out attenuated by sRGB(α) instead of α.
    // Matches the screenshot path's un-premultiplication so the live
    // window and reference PNGs agree on color.
    //
    // The translucent branch goes through `UNPREMUL_LUT` to avoid a
    // per-channel integer divide on every non-edge pixel — the divide
    // dominated this loop's CPU on a 600×400 editor at 2× scale
    // (480k pixels per frame, 3 divides each = 1.4M divides/frame).
    //
    // `clear` + `extend_from_slice` instead of `resize(..., 0)` +
    // index-write: the resize-with-fill seeds new bytes with a
    // sentinel that's never observed (every byte is overwritten on
    // the next line), but if it ever leaked through it'd be wrong in
    // a different way for each fill value (0 = transparent black,
    // 255 = opaque white), so we just don't write a sentinel at all.
    rgba_buf.clear();
    rgba_buf.reserve(pixel_count * 4);
    for px in px_buf.iter() {
        let bytes = if px.alpha == 0 {
            [0, 0, 0, 0]
        } else if px.alpha == 255 {
            [px.red, px.green, px.blue, 255]
        } else {
            let inv_a = UNPREMUL_LUT[px.alpha as usize];
            [
                ((px.red as u32 * inv_a) >> 16) as u8,
                ((px.green as u32 * inv_a) >> 16) as u8,
                ((px.blue as u32 * inv_a) >> 16) as u8,
                px.alpha,
            ]
        };
        rgba_buf.extend_from_slice(&bytes);
    }
}
