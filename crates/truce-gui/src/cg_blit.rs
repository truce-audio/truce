//! CoreGraphics pixel blit — sets RGBA pixels as CALayer contents.
//!
//! Used on macOS instead of wgpu to avoid Metal autorelease pool issues
//! that cause crashes when multiple plugin editor windows coexist (AAX).
//!
//! Uses `[layer setContents: cgImage]` (same shape as JUCE) instead of
//! `drawRect:`. The caller MUST wrap `blit()` in `@autoreleasepool` so
//! the autoreleased `__NSSingleObjectArrayI` wrappers created by
//! `setContents:` drain immediately — never escaping into the host's
//! per-callout ARP.
//!
//! # Performance shape (see aax-gui-performance.md)
//!
//! The goal is to hand pixels to the CoreAnimation compositor without
//! allocating or memcpying each frame, so Pro Tools' main thread stays
//! cheap even with several editor windows open. Concretely:
//!
//! - A single `CGColorSpace` is cached across frames.
//! - `CGDataProviderCreateWithData` is used instead of
//!   `CFDataCreate` + `CGDataProviderCreateWithCFData`: the pixel
//!   buffer is Rust-owned and handed to Core Graphics by pointer. No
//!   per-frame copy into a CFData.
//! - A small pool of pixel buffers ping-pongs between us and the
//!   compositor. The provider's release callback drops a buffer back
//!   into the pool when Core Graphics is finished with the image, so
//!   a new frame can reuse that allocation without touching the system
//!   allocator.
//!
//! The compositor still DMAs pixel data to its GPU textures before
//! display — Core Graphics doesn't hand out raw pointers for GPU reads.
//! For a true GPU-only path (zero CPU touch post-rasterize), we'd need
//! IOSurface-backed contents plus a resolved multi-editor autorelease
//! crash; see `aax-gpu-gui.md` for that plan. This module is the
//! best we can do while staying on plain `CALayer` contents.

#[cfg(target_os = "macos")]
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGColorSpaceRelease(space: *mut c_void);
    fn CGDataProviderCreateWithData(
        info: *mut c_void,
        data: *const u8,
        size: usize,
        release_data: Option<unsafe extern "C" fn(*mut c_void, *const c_void, usize)>,
    ) -> *mut c_void;
    fn CGDataProviderRelease(provider: *mut c_void);
    fn CGImageCreate(
        width: usize,
        height: usize,
        bits_per_component: usize,
        bits_per_pixel: usize,
        bytes_per_row: usize,
        space: *mut c_void,
        bitmap_info: u32,
        provider: *mut c_void,
        decode: *const f64,
        should_interpolate: bool,
        intent: u32,
    ) -> *mut c_void;
    fn CGImageRelease(image: *mut c_void);
}

#[cfg(target_os = "macos")]
const ALPHA_PREMULTIPLIED_LAST: u32 = 1;

/// Capped recycle depth for the pixel-buffer pool. In steady state two
/// buffers cycle between us and the compositor; a third accommodates
/// brief back-pressure if the compositor holds more than one frame at
/// once. Beyond this we'd rather allocate a fresh buffer than grow
/// the pool unboundedly.
#[cfg(target_os = "macos")]
const MAX_POOLED_BUFFERS: usize = 4;

/// Shared pool: buffers the releaser callback drops back into.
///
/// Wrapped in `Arc<Mutex<…>>` because the Core Graphics releaser fires
/// from whatever thread owned the last CGImage reference — usually the
/// GUI thread, but not guaranteed.
#[cfg(target_os = "macos")]
type BufferPool = Arc<Mutex<Vec<Vec<u8>>>>;

/// Info passed to the CG releaser callback — shared pool handle plus
/// the buffer it owns. Allocated per-frame with `Box::into_raw` and
/// freed inside the callback.
#[cfg(target_os = "macos")]
struct ProviderSlot {
    pool: BufferPool,
    buffer: Vec<u8>,
}

/// Called by Core Graphics when the data provider is deallocated.
/// Returns the pixel buffer to the pool so the next frame can reuse
/// its allocation.
#[cfg(target_os = "macos")]
unsafe extern "C" fn release_data_callback(
    info: *mut c_void,
    _data: *const c_void,
    _size: usize,
) {
    if info.is_null() {
        return;
    }
    let slot: ProviderSlot = *Box::from_raw(info as *mut ProviderSlot);
    let ProviderSlot { pool, buffer } = slot;
    {
        let mut guard = match pool.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if guard.len() < MAX_POOLED_BUFFERS {
            guard.push(buffer);
        }
    }
}

/// Blits RGBA pixel data to an NSView's backing CALayer via setContents:.
///
/// Must be called inside @autoreleasepool to drain the autoreleased
/// wrapper objects that setContents: creates internally.
#[cfg(target_os = "macos")]
pub struct CgBlit {
    ns_view: *mut c_void,
    width: u32,
    height: u32,
    /// Cached per-instance — creating a DeviceRGB colorspace is cheap
    /// individually but we do it 30–60 times/sec across every open
    /// editor; caching cuts the Core Graphics retain/release traffic.
    colorspace: *mut c_void,
    /// Ring of pixel buffers. Empty most of the time — the active
    /// frame's buffer is in CoreGraphics' hands and will be returned
    /// via the release callback when the compositor is done.
    pool: BufferPool,
}

// SAFETY: The NSView and CGColorSpace pointers are only written from
// the GUI thread. The buffer pool is explicitly Mutex-protected because
// the CG releaser callback can run from a background thread when the
// compositor drops its last CGImage reference.
#[cfg(target_os = "macos")]
unsafe impl Send for CgBlit {}

#[cfg(target_os = "macos")]
impl CgBlit {
    pub fn new(ns_view: *mut c_void, width: u32, height: u32) -> Self {
        let colorspace = unsafe { CGColorSpaceCreateDeviceRGB() };
        Self {
            ns_view,
            width,
            height,
            colorspace,
            pool: Arc::new(Mutex::new(Vec::with_capacity(MAX_POOLED_BUFFERS))),
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        // Invalidate pooled buffers — they're the old stride. New
        // allocations will size themselves to the new dimensions.
        if let Ok(mut pool) = self.pool.lock() {
            pool.clear();
        }
    }

    /// Blit RGBA premultiplied pixel data to the view's layer.
    ///
    /// Creates a zero-copy CGImage from a pooled buffer and sets it as
    /// the layer's contents. `CATransaction` disables implicit
    /// animations. The caller must wrap this in `@autoreleasepool`.
    pub fn blit(&mut self, pixels: &[u8]) {
        let w = self.width as usize;
        let h = self.height as usize;
        let expected = w * h * 4;
        if pixels.len() < expected || self.ns_view.is_null() || self.colorspace.is_null() {
            return;
        }

        // Take a buffer from the pool, or allocate one if the pool is
        // empty (first frame, or compositor still holds the previous
        // ones). The releaser puts it back when Core Graphics is done.
        let mut buffer = {
            let mut pool = match self.pool.lock() {
                Ok(p) => p,
                // Mutex poisoning → fall back to a fresh allocation.
                Err(p) => p.into_inner(),
            };
            pool.pop().unwrap_or_else(|| Vec::with_capacity(expected))
        };
        if buffer.len() != expected {
            buffer.resize(expected, 0);
        }
        buffer[..expected].copy_from_slice(&pixels[..expected]);

        unsafe {
            use objc::{class, msg_send, sel, sel_impl};

            // Hand ownership of the pixel buffer to Core Graphics via
            // a heap-allocated slot. `release_data_callback` frees the
            // slot and returns the buffer to the pool.
            let slot = Box::new(ProviderSlot {
                pool: self.pool.clone(),
                buffer,
            });
            let slot_ptr = Box::into_raw(slot);
            // The buffer's pointer is stable because `Vec`'s buffer
            // address doesn't move unless the vec itself is reallocated
            // — and nothing touches it between here and the CG callback.
            let data_ptr = (*slot_ptr).buffer.as_ptr();

            let dp = CGDataProviderCreateWithData(
                slot_ptr as *mut c_void,
                data_ptr,
                expected,
                Some(release_data_callback),
            );
            if dp.is_null() {
                // Provider creation failed — free the slot ourselves.
                drop(Box::from_raw(slot_ptr));
                return;
            }

            let image = CGImageCreate(
                w,
                h,
                8,      // bits per component
                32,     // bits per pixel
                w * 4,  // bytes per row
                self.colorspace,
                ALPHA_PREMULTIPLIED_LAST,
                dp,
                std::ptr::null(),
                false,
                0, // kCGRenderingIntentDefault
            );

            CGDataProviderRelease(dp);

            if !image.is_null() {
                let view = self.ns_view as cocoa::base::id;
                let layer: cocoa::base::id = msg_send![view, layer];

                if !layer.is_null() {
                    // Disable implicit CALayer animation (0.25s crossfade)
                    let _: () = msg_send![class!(CATransaction), begin];
                    let _: () = msg_send![class!(CATransaction), setDisableActions: true];
                    let _: () = msg_send![layer, setContents: image as cocoa::base::id];
                    let _: () = msg_send![class!(CATransaction), commit];
                }

                // Layer retains the image via setContents:, release our ref.
                CGImageRelease(image);
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for CgBlit {
    fn drop(&mut self) {
        unsafe {
            if !self.colorspace.is_null() {
                CGColorSpaceRelease(self.colorspace);
                self.colorspace = std::ptr::null_mut();
            }
        }
    }
}
