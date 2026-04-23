//! CPU rendering backend using tiny-skia.
//!
//! Renders to an in-memory RGBA pixel buffer (premultiplied alpha,
//! row-major) sized at `logical × scale` physical pixels. Callers
//! draw in **logical points**; the backend multiplies input
//! coordinates by `scale` internally, matching the contract of
//! [`WgpuBackend`](../../truce_gpu/struct.WgpuBackend.html) so text
//! and primitives stay sharp on Retina displays.

use tiny_skia::{Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};

use crate::render::{ImageId, RenderBackend};
use crate::theme::Color;

/// CPU-based rendering backend.
///
/// Wraps a tiny-skia `Pixmap` and implements `RenderBackend` using
/// software rasterization. Zero GPU dependencies.
pub struct CpuBackend {
    pixmap: Pixmap,
    /// Display scale factor: `logical × scale = physical`. Applied
    /// inside every `RenderBackend` method so callers author in
    /// logical points.
    scale: f32,
    /// Registered images. Index = ImageId.0. None = unregistered slot.
    images: Vec<Option<Pixmap>>,
}

impl CpuBackend {
    /// Create a new CPU backend.
    ///
    /// `logical_w` / `logical_h` are in logical points; `scale` is the
    /// display scale factor (2.0 on Retina, 1.0 otherwise). The
    /// internal pixmap is sized at `logical × scale` physical pixels.
    pub fn new(logical_w: u32, logical_h: u32, scale: f32) -> Option<Self> {
        let scale = scale.max(0.0);
        let phys_w = ((logical_w.max(1) as f32) * scale).round().max(1.0) as u32;
        let phys_h = ((logical_h.max(1) as f32) * scale).round().max(1.0) as u32;
        Pixmap::new(phys_w, phys_h).map(|pixmap| Self {
            pixmap,
            scale,
            images: Vec::new(),
        })
    }

    /// Reallocate the internal pixmap for a new logical size and/or
    /// scale factor. Call from a host-reported resize / DPI-change
    /// handler; a no-op if the resulting physical dimensions match
    /// the current pixmap.
    pub fn resize(&mut self, logical_w: u32, logical_h: u32, scale: f32) -> bool {
        let scale = scale.max(0.0);
        let phys_w = ((logical_w.max(1) as f32) * scale).round().max(1.0) as u32;
        let phys_h = ((logical_h.max(1) as f32) * scale).round().max(1.0) as u32;
        if phys_w == self.pixmap.width() && phys_h == self.pixmap.height() {
            self.scale = scale;
            return false;
        }
        match Pixmap::new(phys_w, phys_h) {
            Some(pm) => {
                self.pixmap = pm;
                self.scale = scale;
                true
            }
            None => false,
        }
    }

    /// Raw pixel data (RGBA premultiplied, row-major, physical pixels).
    pub fn data(&self) -> &[u8] {
        self.pixmap.data()
    }

    /// Pixel buffer width (physical pixels).
    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }

    /// Pixel buffer height (physical pixels).
    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }

    /// Display scale factor baked at construction.
    pub fn scale(&self) -> f32 {
        self.scale
    }
}

/// All `RenderBackend` methods accept coordinates in **logical
/// points**. The backend multiplies by `self.scale` before handing
/// off to tiny-skia, so the pixmap is rasterized at physical-pixel
/// density.
impl RenderBackend for CpuBackend {
    fn clear(&mut self, color: Color) {
        self.pixmap.fill(color.to_skia());
    }

    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        let s = self.scale;
        let rect = match tiny_skia::Rect::from_xywh(x * s, y * s, w * s, h * s) {
            Some(r) => r,
            None => return,
        };
        let mut paint = Paint::default();
        paint.set_color(color.to_skia());
        paint.anti_alias = true;
        self.pixmap
            .fill_rect(rect, &paint, Transform::identity(), None);
    }

    fn fill_circle(&mut self, cx: f32, cy: f32, radius: f32, color: Color) {
        let s = self.scale;
        let mut pb = PathBuilder::new();
        pb.push_circle(cx * s, cy * s, radius * s);
        let path = match pb.finish() {
            Some(p) => p,
            None => return,
        };
        let mut paint = Paint::default();
        paint.set_color(color.to_skia());
        paint.anti_alias = true;
        self.pixmap.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    fn stroke_circle(&mut self, cx: f32, cy: f32, radius: f32, color: Color, width: f32) {
        let s = self.scale;
        let mut pb = PathBuilder::new();
        pb.push_circle(cx * s, cy * s, radius * s);
        let path = match pb.finish() {
            Some(p) => p,
            None => return,
        };
        let mut paint = Paint::default();
        paint.set_color(color.to_skia());
        paint.anti_alias = true;
        let stroke = Stroke {
            width: width * s,
            ..Stroke::default()
        };
        self.pixmap
            .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    fn stroke_arc(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        start_angle: f32,
        end_angle: f32,
        color: Color,
        width: f32,
    ) {
        let s = self.scale;
        let segments = 64;
        let mut pb = PathBuilder::new();
        let angle_range = end_angle - start_angle;
        let step = angle_range / segments as f32;

        for i in 0..=segments {
            let angle = start_angle + step * i as f32;
            let x = cx * s + radius * s * angle.cos();
            let y = cy * s + radius * s * angle.sin();
            if i == 0 {
                pb.move_to(x, y);
            } else {
                pb.line_to(x, y);
            }
        }

        let path = match pb.finish() {
            Some(p) => p,
            None => return,
        };
        let mut paint = Paint::default();
        paint.set_color(color.to_skia());
        paint.anti_alias = true;
        let stroke = Stroke {
            width: width * s,
            line_cap: tiny_skia::LineCap::Round,
            ..Stroke::default()
        };
        self.pixmap
            .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    fn draw_line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, color: Color, width: f32) {
        let s = self.scale;
        let mut pb = PathBuilder::new();
        pb.move_to(x1 * s, y1 * s);
        pb.line_to(x2 * s, y2 * s);
        let path = match pb.finish() {
            Some(p) => p,
            None => return,
        };
        let mut paint = Paint::default();
        paint.set_color(color.to_skia());
        paint.anti_alias = true;
        let stroke = Stroke {
            width: width * s,
            line_cap: tiny_skia::LineCap::Round,
            ..Stroke::default()
        };
        self.pixmap
            .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    fn draw_text(&mut self, text: &str, x: f32, y: f32, size: f32, color: Color) {
        let s = self.scale;
        let w = self.pixmap.width();
        let h = self.pixmap.height();
        crate::font::draw_text_fontdue(
            self.pixmap.data_mut(),
            w, h,
            text, x * s, y * s, size * s,
            color.r, color.g, color.b, color.a,
        );
    }

    fn text_width(&self, text: &str, size: f32) -> f32 {
        let s = self.scale;
        crate::font::text_width_fontdue(text, size * s) / s
    }

    fn register_image(&mut self, rgba: &[u8], width: u32, height: u32) -> ImageId {
        let mut pm = match Pixmap::new(width, height) {
            Some(p) => p,
            None => return ImageId::INVALID,
        };
        let expected = (width as usize) * (height as usize) * 4;
        if rgba.len() < expected {
            return ImageId::INVALID;
        }
        pm.data_mut()[..expected].copy_from_slice(&rgba[..expected]);

        if let Some(slot) = self.images.iter_mut().enumerate()
            .find(|(_, s)| s.is_none())
        {
            *slot.1 = Some(pm);
            return ImageId(slot.0 as u32);
        }
        let id = self.images.len() as u32;
        self.images.push(Some(pm));
        ImageId(id)
    }

    fn unregister_image(&mut self, id: ImageId) {
        if let Some(slot) = self.images.get_mut(id.0 as usize) {
            *slot = None;
        }
    }

    fn draw_image(&mut self, id: ImageId, x: f32, y: f32, w: f32, h: f32) {
        let s = self.scale;
        let pm = match self.images.get(id.0 as usize).and_then(|s| s.as_ref()) {
            Some(p) => p,
            None => return,
        };
        let sx = (w * s) / pm.width() as f32;
        let sy = (h * s) / pm.height() as f32;
        let transform = Transform::from_scale(sx, sy).post_translate(x * s, y * s);
        let paint = PixmapPaint::default();
        self.pixmap.draw_pixmap(0, 0, pm.as_ref(), &paint, transform, None);
    }
}
