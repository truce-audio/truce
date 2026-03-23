//! CPU rendering backend using tiny-skia.
//!
//! Renders to an in-memory RGBA pixel buffer (premultiplied alpha, row-major).

use tiny_skia::{Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::render::RenderBackend;
use crate::theme::Color;

/// CPU-based rendering backend.
///
/// Wraps a tiny-skia `Pixmap` and implements `RenderBackend` using
/// software rasterization. Zero GPU dependencies.
pub struct CpuBackend {
    pixmap: Pixmap,
}

impl CpuBackend {
    /// Create a new CPU backend with the given pixel dimensions.
    pub fn new(width: u32, height: u32) -> Option<Self> {
        Pixmap::new(width, height).map(|pixmap| Self { pixmap })
    }

    /// Raw pixel data (RGBA premultiplied, row-major).
    pub fn data(&self) -> &[u8] {
        self.pixmap.data()
    }

    /// Pixel buffer width.
    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }

    /// Pixel buffer height.
    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }
}

impl RenderBackend for CpuBackend {
    fn clear(&mut self, color: Color) {
        self.pixmap.fill(color.to_skia());
    }

    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        let rect = match tiny_skia::Rect::from_xywh(x, y, w, h) {
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
        let mut pb = PathBuilder::new();
        pb.push_circle(cx, cy, radius);
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
        let mut pb = PathBuilder::new();
        pb.push_circle(cx, cy, radius);
        let path = match pb.finish() {
            Some(p) => p,
            None => return,
        };
        let mut paint = Paint::default();
        paint.set_color(color.to_skia());
        paint.anti_alias = true;
        let stroke = Stroke {
            width,
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
        let segments = 64;
        let mut pb = PathBuilder::new();
        let angle_range = end_angle - start_angle;
        let step = angle_range / segments as f32;

        for i in 0..=segments {
            let angle = start_angle + step * i as f32;
            let x = cx + radius * angle.cos();
            let y = cy + radius * angle.sin();
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
            width,
            line_cap: tiny_skia::LineCap::Round,
            ..Stroke::default()
        };
        self.pixmap
            .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    fn draw_line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, color: Color, width: f32) {
        let mut pb = PathBuilder::new();
        pb.move_to(x1, y1);
        pb.line_to(x2, y2);
        let path = match pb.finish() {
            Some(p) => p,
            None => return,
        };
        let mut paint = Paint::default();
        paint.set_color(color.to_skia());
        paint.anti_alias = true;
        let stroke = Stroke {
            width,
            line_cap: tiny_skia::LineCap::Round,
            ..Stroke::default()
        };
        self.pixmap
            .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    fn draw_text(&mut self, text: &str, x: f32, y: f32, size: f32, color: Color) {
        let w = self.pixmap.width();
        let h = self.pixmap.height();
        crate::font::draw_text_fontdue(
            self.pixmap.data_mut(),
            w, h,
            text, x, y, size,
            color.r, color.g, color.b, color.a,
        );
    }

    fn text_width(&self, text: &str, size: f32) -> f32 {
        crate::font::text_width_fontdue(text, size)
    }
}
