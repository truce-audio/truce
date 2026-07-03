//! Font rendering over the shared glyph rasterizer
//! (`truce_font::raster`, skrifa + tiny-skia).
//!
//! The bundled `JetBrains` Mono ships in the dedicated `truce-font`
//! crate. Advanced users can override the bundled font via Cargo's
//! `[patch]` table on `truce-font` instead of forking `truce-gui`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::LazyLock;

pub use truce_font::JETBRAINS_MONO;

/// Cached rasterized glyph.
struct CachedGlyph {
    bitmap: Vec<u8>, // alpha values, row-major
    width: u32,
    height: u32,
    advance: f32,  // horizontal advance in pixels
    y_offset: f32, // offset from baseline (negative = above baseline)
}

struct GlyphCache {
    font: truce_font::raster::FontRaster,
    glyphs: HashMap<(char, u32), CachedGlyph>,
}

// Per-thread glyph cache. A single shared `Mutex<Option<GlyphCache>>`
// would force every `draw_text` call through a lock, contending
// across multi-instance hosts that drive plugin UIs from different
// threads. Each thread lazy-inits its own cache instead; the font
// bytes are `'static` (re-exported from `truce-font`) so the
// per-thread duplication only covers parsed font tables and
// rasterized glyphs (one per `(char, size)` the thread has drawn).
thread_local! {
    static CACHE: RefCell<Option<GlyphCache>> = const { RefCell::new(None) };
}

fn with_cache<R>(f: impl FnOnce(&mut GlyphCache) -> R) -> R {
    CACHE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let cache = guard.get_or_insert_with(|| GlyphCache {
            font: truce_font::raster::FontRaster::new(),
            glyphs: HashMap::new(),
        });
        f(cache)
    })
}

// Quantized cache key (one decimal place). The truncation is the
// quantization's whole point - `12.34` and `12.36` both → `123`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn size_key(size: f32) -> u32 {
    (size * 10.0) as u32
}

/// Rasterize and cache a glyph, returning its cached data.
fn get_glyph(cache: &mut GlyphCache, ch: char, size: f32) -> &CachedGlyph {
    let key = (ch, size_key(size));
    let GlyphCache { font, glyphs } = cache;
    glyphs.entry(key).or_insert_with(|| {
        let glyph = font.rasterize(ch, size);
        CachedGlyph {
            bitmap: glyph.coverage,
            width: glyph.width,
            height: glyph.height,
            advance: glyph.advance,
            y_offset: glyph.y_min,
        }
    })
}

/// sRGB-to-linear lookup for byte-encoded color channels. Used by
/// `draw_text` to composite glyphs in linear space - see the
/// gamma rationale on that function.
#[allow(clippy::cast_precision_loss)]
static SRGB_TO_LINEAR: LazyLock<[f32; 256]> = LazyLock::new(|| {
    let mut table = [0.0f32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let s = i as f32 / 255.0;
        *slot = if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        };
    }
    table
});

#[inline]
fn srgb_f32_to_linear(s: f32) -> f32 {
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

#[inline]
// `lin` is clamped to `[0, 1]`; the sRGB curve produces ≤ 1.0 for
// every clamped input, so `s * 255.0 + 0.5` lands in `[0, 255.5]`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn linear_to_srgb_u8(lin: f32) -> u8 {
    let lin = lin.clamp(0.0, 1.0);
    let s = if lin <= 0.003_130_8 {
        12.92 * lin
    } else {
        1.055 * lin.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5) as u8
}

/// Draw text into an RGBA pixel buffer.
///
/// Compositing happens in linear space: destination bytes are decoded
/// sRGB → linear via a 256-entry lookup table, the source color is decoded
/// the same way, the Porter-Duff "over" operator runs in linear (so a
/// half-coverage pixel against opaque white produces a perceptual
/// midtone, not the gamma-darkened midtone of naive sRGB blending),
/// and the result is re-encoded to sRGB. Treats the destination as
/// straight sRGB rather than sRGB-premultiplied - fully correct when
/// the destination alpha is 1 (the dominant case for text rendering),
/// approximate when the destination is itself translucent. The rest
/// of the CPU backend uses tiny-skia which is sRGB-naive too, so a
/// fully gamma-correct pipeline would need matching changes there.
///
/// Glyph caching is internal - first call for a given (char, size)
/// pair rasterizes; subsequent calls blit from the per-thread cache.
//
// Glyph dimensions widen `u32 as f32`. Glyph bitmaps are tens of
// pixels wide - far below 2^23 / 2^31. The bounds-checked
// `i32 -> u32` indexing already guards against negative values.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names
)]
pub fn draw_text(
    pixmap_data: &mut [u8],
    pixmap_width: u32,
    pixmap_height: u32,
    text: &str,
    x: f32,
    y: f32,
    size: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) {
    with_cache(|cache| {
        let mut cursor_x = x;

        let ascent = cache.font.ascent(size);

        // Pre-compute the source color in linear space; only the
        // glyph-coverage alpha varies per pixel.
        let src_lin_r = srgb_f32_to_linear(r.clamp(0.0, 1.0));
        let src_lin_g = srgb_f32_to_linear(g.clamp(0.0, 1.0));
        let src_lin_b = srgb_f32_to_linear(b.clamp(0.0, 1.0));

        for ch in text.chars() {
            let glyph = get_glyph(cache, ch, size);
            let gw = glyph.width;
            let gh = glyph.height;

            // Glyph coordinates fit in i32 (window is < 32k px).
            #[allow(clippy::cast_possible_truncation)]
            let gx = cursor_x as i32;
            #[allow(clippy::cast_possible_truncation)]
            let gy = (y + ascent - glyph.y_offset - gh as f32) as i32;

            for row in 0..gh {
                for col in 0..gw {
                    let px = gx + col as i32;
                    let py = gy + row as i32;

                    if px < 0 || py < 0 || px >= pixmap_width as i32 || py >= pixmap_height as i32 {
                        continue;
                    }

                    let coverage = glyph.bitmap[(row * gw + col) as usize];
                    if coverage == 0 {
                        continue;
                    }

                    let ga = (f32::from(coverage) / 255.0) * a;
                    let idx = ((py as u32 * pixmap_width + px as u32) * 4) as usize;
                    if idx + 3 >= pixmap_data.len() {
                        continue;
                    }

                    // Decode dst sRGB → linear, blend Porter-Duff over
                    // (premultiplied source), encode back. Alpha stays
                    // linear by definition (it's a coverage value, not
                    // a perceptual signal).
                    let dst_lin_r = SRGB_TO_LINEAR[pixmap_data[idx] as usize];
                    let dst_lin_g = SRGB_TO_LINEAR[pixmap_data[idx + 1] as usize];
                    let dst_lin_b = SRGB_TO_LINEAR[pixmap_data[idx + 2] as usize];
                    let dst_a = f32::from(pixmap_data[idx + 3]) / 255.0;

                    let inv_sa = 1.0 - ga;
                    let out_lin_r = src_lin_r * ga + dst_lin_r * inv_sa;
                    let out_lin_g = src_lin_g * ga + dst_lin_g * inv_sa;
                    let out_lin_b = src_lin_b * ga + dst_lin_b * inv_sa;
                    let out_a = ga + dst_a * inv_sa;

                    pixmap_data[idx] = linear_to_srgb_u8(out_lin_r);
                    pixmap_data[idx + 1] = linear_to_srgb_u8(out_lin_g);
                    pixmap_data[idx + 2] = linear_to_srgb_u8(out_lin_b);
                    // `out_a` is bounded in `[0, 1]` (alpha-blended).
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let out_a_u8 = (out_a * 255.0 + 0.5) as u8;
                    pixmap_data[idx + 3] = out_a_u8;
                }
            }

            cursor_x += glyph.advance;
        }
    });
}

/// Measure text width in pixels.
#[must_use]
pub fn text_width(text: &str, size: f32) -> f32 {
    with_cache(|cache| {
        let mut width = 0.0f32;
        for ch in text.chars() {
            let glyph = get_glyph(cache, ch, size);
            width += glyph.advance;
        }
        width
    })
}
