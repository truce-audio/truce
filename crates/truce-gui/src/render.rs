//! Render backend trait for abstracting over CPU and GPU rendering.
//!
//! Widgets draw through this trait, making them backend-agnostic.

use crate::theme::Color;

/// Opaque handle to a backend-registered image.
///
/// Returned by [`RenderBackend::register_image`]; passed to
/// [`RenderBackend::draw_image`]. Valid until `unregister_image` or the
/// backend is dropped. Two backends do not share ids — ids from one
/// backend must not be used with another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageId(pub u32);

impl ImageId {
    /// Sentinel value returned by the default trait impls when a backend
    /// does not override `register_image`.
    pub const INVALID: Self = Self(u32::MAX);
}

/// Abstraction over rendering backends (CPU via tiny-skia, future GPU via wgpu).
///
/// All coordinates are in pixels. The CPU backend renders to an in-memory
/// RGBA buffer; a GPU backend would render via Metal/DX12/Vulkan.
pub trait RenderBackend {
    /// Clear the entire surface with a solid color.
    fn clear(&mut self, color: Color);

    /// Fill a rectangle.
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color);

    /// Fill a circle.
    fn fill_circle(&mut self, cx: f32, cy: f32, radius: f32, color: Color);

    /// Stroke a circle outline.
    fn stroke_circle(&mut self, cx: f32, cy: f32, radius: f32, color: Color, width: f32);

    /// Stroke an arc (portion of a circle).
    fn stroke_arc(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        start_angle: f32,
        end_angle: f32,
        color: Color,
        width: f32,
    );

    /// Draw a line between two points.
    fn draw_line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, color: Color, width: f32);

    /// Draw text using the embedded TrueType font (fontdue).
    fn draw_text(&mut self, text: &str, x: f32, y: f32, size: f32, color: Color);

    /// Measure the width of a text string in pixels.
    fn text_width(&self, text: &str, size: f32) -> f32;

    /// Flush rendering to the display surface.
    ///
    /// No-op for CPU backends (pixels are read directly from the buffer).
    /// GPU backends submit their command buffer and present here.
    fn present(&mut self) {}

    /// Register an RGBA8 image (premultiplied alpha, row-major, tightly packed).
    ///
    /// Returned id is valid until `unregister_image` is called or the backend
    /// is dropped. Returns [`ImageId::INVALID`] if the backend does not
    /// support images.
    fn register_image(&mut self, _rgba: &[u8], _width: u32, _height: u32) -> ImageId {
        ImageId::INVALID
    }

    /// Remove a previously-registered image. No-op if the id is invalid
    /// or already unregistered.
    fn unregister_image(&mut self, _id: ImageId) {}

    /// Draw a previously-registered image at `(x, y)` sized `w × h`.
    ///
    /// Sampling is linear. The image is scaled to fill the target rect.
    /// The default impl draws a magenta rect so missing backend support
    /// is visually obvious.
    fn draw_image(&mut self, _id: ImageId, x: f32, y: f32, w: f32, h: f32) {
        self.fill_rect(x, y, w, h, Color::rgb(1.0, 0.0, 1.0));
    }
}
