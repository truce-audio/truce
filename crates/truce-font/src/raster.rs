//! Glyph rasterization over the bundled font (skrifa outlines filled
//! with tiny-skia). Shared by the CPU and GPU render backends so both
//! produce identical coverage bitmaps and metrics.
//!
//! Coordinate conventions match what the backends' blit math expects:
//! `y_min` is the distance from the baseline up to the bitmap's bottom
//! edge (negative when the glyph descends), so the bitmap's top row
//! sits at `ascent - (y_min + height)` below the top of the line box.

use skrifa::instance::{LocationRef, Size};
use skrifa::metrics::GlyphMetrics;
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, GlyphId, MetadataProvider};
use tiny_skia::{FillRule, Mask, PathBuilder, Transform};

use crate::JETBRAINS_MONO;

/// One rasterized glyph: an A8 coverage bitmap plus placement metrics.
pub struct GlyphRaster {
    /// Coverage values (0..=255), row-major, `width * height` bytes.
    pub coverage: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Horizontal advance in pixels.
    pub advance: f32,
    /// Baseline-relative offset of the bitmap's bottom edge, y-up
    /// (negative when the glyph descends below the baseline).
    pub y_min: f32,
}

/// Rasterizer over the bundled `JetBrains` Mono. Parse once, then
/// rasterize per `(char, size)`; callers cache the results.
pub struct FontRaster {
    font: FontRef<'static>,
}

impl Default for FontRaster {
    fn default() -> Self {
        Self::new()
    }
}

impl FontRaster {
    /// Parse the bundled font.
    ///
    /// # Panics
    /// If the embedded font bytes fail to parse - the bytes are
    /// compiled in, so that is a build defect, not a runtime
    /// condition.
    #[must_use]
    pub fn new() -> Self {
        let font = FontRef::new(JETBRAINS_MONO).expect("failed to parse embedded font");
        Self { font }
    }

    /// Ascent in pixels at `size`: baseline distance from the top of
    /// the line box.
    #[must_use]
    pub fn ascent(&self, size: f32) -> f32 {
        self.font
            .metrics(Size::new(size), LocationRef::default())
            .ascent
    }

    /// Horizontal advance in pixels for `ch` at `size`.
    #[must_use]
    pub fn advance(&self, ch: char, size: f32) -> f32 {
        let gid = self.glyph_id(ch);
        GlyphMetrics::new(&self.font, Size::new(size), LocationRef::default())
            .advance_width(gid)
            .unwrap_or(0.0)
    }

    /// Rasterize `ch` at `size` pixels. Glyphs with no outline (space)
    /// come back as a `0x0` bitmap carrying only the advance.
    #[must_use]
    pub fn rasterize(&self, ch: char, size: f32) -> GlyphRaster {
        let gid = self.glyph_id(ch);
        let advance = GlyphMetrics::new(&self.font, Size::new(size), LocationRef::default())
            .advance_width(gid)
            .unwrap_or(0.0);

        let mut pen = TinySkiaPen {
            builder: PathBuilder::new(),
        };
        if let Some(outline) = self.font.outline_glyphs().get(gid) {
            let settings = DrawSettings::unhinted(Size::new(size), LocationRef::default());
            let _ = outline.draw(settings, &mut pen);
        }

        let Some(path) = pen.builder.finish() else {
            return GlyphRaster {
                coverage: Vec::new(),
                width: 0,
                height: 0,
                advance,
                y_min: 0.0,
            };
        };

        // Integer-align the bitmap on the outline's bounding box, then
        // fill the path translated onto it. Glyph boxes are tens of
        // pixels; the float -> u32 conversions can't overflow.
        let bounds = path.bounds();
        let x0 = bounds.left().floor();
        let y0 = bounds.top().floor();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let width = (bounds.right().ceil() - x0).max(1.0) as u32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let height = (bounds.bottom().ceil() - y0).max(1.0) as u32;
        let Some(mut mask) = Mask::new(width, height) else {
            return GlyphRaster {
                coverage: Vec::new(),
                width: 0,
                height: 0,
                advance,
                y_min: 0.0,
            };
        };
        mask.fill_path(
            &path,
            FillRule::Winding,
            true,
            Transform::from_translate(-x0, -y0),
        );

        // The path is y-down (see `TinySkiaPen`), so the bitmap's
        // bottom edge sits at `y0 + height`; negate back to the y-up
        // baseline-relative offset the blit math expects.
        #[allow(clippy::cast_precision_loss)]
        let y_min = -(y0 + height as f32);

        GlyphRaster {
            coverage: mask.data().to_vec(),
            width,
            height,
            advance,
            y_min,
        }
    }

    /// Character -> glyph id; unmapped characters fall back to `.notdef`
    /// (glyph 0), which renders the font's tofu box.
    fn glyph_id(&self, ch: char) -> GlyphId {
        self.font.charmap().map(ch).unwrap_or(GlyphId::new(0))
    }
}

/// Collects skrifa's y-up outline callbacks into a y-down tiny-skia
/// path so the rasterized rows come out top-to-bottom.
struct TinySkiaPen {
    builder: PathBuilder,
}

impl OutlinePen for TinySkiaPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.builder.move_to(x, -y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(x, -y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.builder.quad_to(cx0, -cy0, x, -y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.builder.cubic_to(cx0, -cy0, cx1, -cy1, x, -y);
    }

    fn close(&mut self) {
        self.builder.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_glyph_has_coverage_and_metrics() {
        let raster = FontRaster::new();
        let glyph = raster.rasterize('A', 16.0);
        assert!(glyph.width > 0 && glyph.height > 0);
        assert_eq!(glyph.coverage.len(), (glyph.width * glyph.height) as usize);
        assert!(glyph.coverage.iter().any(|&c| c > 0), "some ink");
        assert!(glyph.advance > 0.0);
        // An uppercase letter sits on the baseline.
        assert!(
            glyph.y_min.abs() < 1.5,
            "y_min ~ baseline, got {}",
            glyph.y_min
        );
    }

    #[test]
    fn space_is_advance_only() {
        let raster = FontRaster::new();
        let glyph = raster.rasterize(' ', 16.0);
        assert_eq!((glyph.width, glyph.height), (0, 0));
        assert!(glyph.advance > 0.0);
    }

    #[test]
    fn descender_reaches_below_baseline() {
        let raster = FontRaster::new();
        let glyph = raster.rasterize('g', 16.0);
        assert!(
            glyph.y_min < -1.0,
            "descender below baseline, got {}",
            glyph.y_min
        );
    }

    #[test]
    fn ascent_is_positive_and_scales() {
        let raster = FontRaster::new();
        let a16 = raster.ascent(16.0);
        let a32 = raster.ascent(32.0);
        assert!(a16 > 0.0);
        assert!((a32 / a16 - 2.0).abs() < 0.01, "ascent scales linearly");
    }

    #[test]
    fn monospace_advances_match() {
        let raster = FontRaster::new();
        assert!((raster.advance('i', 14.0) - raster.advance('W', 14.0)).abs() < f32::EPSILON);
    }
}
