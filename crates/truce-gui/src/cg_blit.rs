//! CoreGraphics CPU blit — sets RGBA pixels as CALayer contents.
//!
//! Used on macOS instead of wgpu to avoid Metal autorelease pool issues
//! that cause crashes when multiple plugin editor windows coexist (AAX).
//!
//! Uses `[layer setContents: cgImage]` (same as JUCE) instead of drawRect:.
//! The caller MUST wrap blit() in @autoreleasepool so the autoreleased
//! `__NSSingleObjectArrayI` wrapper objects created by setContents: drain
//! immediately — never escaping into the host's per-callout ARP.

#[cfg(target_os = "macos")]
use std::ffi::c_void;

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDataCreate(allocator: *const c_void, bytes: *const u8, length: isize) -> *mut c_void;
    fn CFRelease(cf: *const c_void);
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGColorSpaceRelease(space: *mut c_void);
    fn CGDataProviderCreateWithCFData(data: *mut c_void) -> *mut c_void;
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

/// Blits RGBA pixel data to an NSView's backing CALayer via setContents:.
///
/// Must be called inside @autoreleasepool to drain the autoreleased
/// wrapper objects that setContents: creates internally.
#[cfg(target_os = "macos")]
pub struct CgBlit {
    ns_view: *mut c_void,
    width: u32,
    height: u32,
}

// SAFETY: Only accessed from the GUI thread.
#[cfg(target_os = "macos")]
unsafe impl Send for CgBlit {}

#[cfg(target_os = "macos")]
impl CgBlit {
    pub fn new(ns_view: *mut c_void, width: u32, height: u32) -> Self {
        Self { ns_view, width, height }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }

    /// Blit RGBA premultiplied pixel data to the view's layer.
    ///
    /// Creates a CGImage from the pixel data and sets it as the layer's
    /// contents. CATransaction disables implicit animations. The caller
    /// must wrap this in @autoreleasepool.
    pub fn blit(&mut self, pixels: &[u8]) {
        let w = self.width as usize;
        let h = self.height as usize;
        let expected = w * h * 4;
        if pixels.len() < expected || self.ns_view.is_null() {
            return;
        }

        unsafe {
            use objc::{class, msg_send, sel, sel_impl};

            // Copy pixels into CFData so the image owns the data
            let cf_data = CFDataCreate(std::ptr::null(), pixels.as_ptr(), expected as isize);
            if cf_data.is_null() {
                return;
            }

            let cs = CGColorSpaceCreateDeviceRGB();
            let dp = CGDataProviderCreateWithCFData(cf_data);
            CFRelease(cf_data);

            let image = CGImageCreate(
                w,
                h,
                8,      // bits per component
                32,     // bits per pixel
                w * 4,  // bytes per row
                cs,
                ALPHA_PREMULTIPLIED_LAST,
                dp,
                std::ptr::null(),
                false,
                0, // kCGRenderingIntentDefault
            );

            CGDataProviderRelease(dp);
            CGColorSpaceRelease(cs);

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

                // Layer retains the image via setContents:, release our ref
                CGImageRelease(image);
            }
        }
    }
}
