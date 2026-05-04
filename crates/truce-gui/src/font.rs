//! Font rendering using fontdue (TrueType rasterization).
//!
//! The bundled JetBrains Mono ships in the dedicated `truce-font`
//! crate; this module re-exports it at the historical
//! `truce_gui::font::JETBRAINS_MONO` path so existing callers keep
//! working. Advanced users can override the bundled font via Cargo's
//! `[patch]` table on `truce-font` instead of forking `truce-gui`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::LazyLock;

/// JetBrains Mono Regular TrueType bytes — re-exported from
/// [`truce_font`] for backwards compatibility. Prefer
/// `truce_font::JETBRAINS_MONO` in new code; both refer to the same
/// `&'static [u8]`.
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
    font: fontdue::Font,
    glyphs: HashMap<(char, u32), CachedGlyph>,
}

// Per-thread glyph cache. The previous global `Mutex<Option<GlyphCache>>`
// took a lock once per draw_text call, which scaled fine for single-host
// use but added avoidable cross-thread contention under multi-instance
// hosts that drive multiple plugin UIs from different threads. Each
// thread now lazy-inits its own cache; the font bytes are `'static`
// (re-exported from `truce-font`) so the per-thread duplication only
// covers parsed font tables and rasterized glyphs — small and bounded
// (one per (char, size) the thread has actually drawn).
thread_local! {
    static CACHE: RefCell<Option<GlyphCache>> = const { RefCell::new(None) };
}

fn with_cache<R>(f: impl FnOnce(&mut GlyphCache) -> R) -> R {
    CACHE.with(|cell| {
        let mut guard = cell.borrow_mut();
        if guard.is_none() {
            let font = fontdue::Font::from_bytes(JETBRAINS_MONO, fontdue::FontSettings::default())
                .expect("failed to parse embedded font");
            *guard = Some(GlyphCache {
                font,
                glyphs: HashMap::new(),
            });
        }
        f(guard.as_mut().unwrap())
    })
}

fn size_key(size: f32) -> u32 {
    (size * 10.0) as u32
}

/// Rasterize and cache a glyph, returning its cached data.
fn get_glyph(cache: &mut GlyphCache, ch: char, size: f32) -> &CachedGlyph {
    let key = (ch, size_key(size));
    if !cache.glyphs.contains_key(&key) {
        let (metrics, bitmap) = cache.font.rasterize(ch, size);
        cache.glyphs.insert(
            key,
            CachedGlyph {
                bitmap,
                width: metrics.width as u32,
                height: metrics.height as u32,
                advance: metrics.advance_width,
                y_offset: metrics.ymin as f32,
            },
        );
    }
    cache.glyphs.get(&key).unwrap()
}

/// sRGB-to-linear lookup for byte-encoded color channels. Used by
/// `draw_text_fontdue` to composite glyphs in linear space — see the
/// gamma rationale on that function.
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
fn linear_to_srgb_u8(lin: f32) -> u8 {
    let lin = lin.clamp(0.0, 1.0);
    let s = if lin <= 0.0031308 {
        12.92 * lin
    } else {
        1.055 * lin.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5) as u8
}

/// Draw text into an RGBA pixel buffer.
///
/// Compositing happens in linear space: destination bytes are decoded
/// sRGB → linear via [`SRGB_TO_LINEAR`], the source color is decoded
/// the same way, the Porter-Duff "over" operator runs in linear (so a
/// half-coverage pixel against opaque white produces a perceptual
/// midtone, not the gamma-darkened midtone of naive sRGB blending),
/// and the result is re-encoded to sRGB. Treats the destination as
/// straight sRGB rather than sRGB-premultiplied — fully correct when
/// the destination alpha is 1 (the dominant case for text rendering),
/// approximate when the destination is itself translucent (the audit's
/// "synthetic test case" — the rest of the CPU backend uses tiny-skia
/// which is sRGB-naive too, so a fully gamma-correct pipeline would
/// need matching changes there).
///
/// Glyph caching is internal — first call for a given (char, size)
/// pair rasterizes; subsequent calls blit from the per-thread cache.
pub fn draw_text_fontdue(
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

        let line_metrics = cache.font.horizontal_line_metrics(size);
        let ascent = line_metrics.map(|m| m.ascent).unwrap_or(size * 0.8);

        // Pre-compute the source color in linear space; only the
        // glyph-coverage alpha varies per pixel.
        let src_lin_r = srgb_f32_to_linear(r.clamp(0.0, 1.0));
        let src_lin_g = srgb_f32_to_linear(g.clamp(0.0, 1.0));
        let src_lin_b = srgb_f32_to_linear(b.clamp(0.0, 1.0));

        for ch in text.chars() {
            let glyph = get_glyph(cache, ch, size);
            let gw = glyph.width;
            let gh = glyph.height;

            let gx = cursor_x as i32;
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

                    let ga = (coverage as f32 / 255.0) * a;
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
                    let dst_a = pixmap_data[idx + 3] as f32 / 255.0;

                    let inv_sa = 1.0 - ga;
                    let out_lin_r = src_lin_r * ga + dst_lin_r * inv_sa;
                    let out_lin_g = src_lin_g * ga + dst_lin_g * inv_sa;
                    let out_lin_b = src_lin_b * ga + dst_lin_b * inv_sa;
                    let out_a = ga + dst_a * inv_sa;

                    pixmap_data[idx] = linear_to_srgb_u8(out_lin_r);
                    pixmap_data[idx + 1] = linear_to_srgb_u8(out_lin_g);
                    pixmap_data[idx + 2] = linear_to_srgb_u8(out_lin_b);
                    pixmap_data[idx + 3] = (out_a * 255.0 + 0.5) as u8;
                }
            }

            cursor_x += glyph.advance;
        }
    });
}

/// Measure text width in pixels.
pub fn text_width_fontdue(text: &str, size: f32) -> f32 {
    with_cache(|cache| {
        let mut width = 0.0f32;
        for ch in text.chars() {
            let glyph = get_glyph(cache, ch, size);
            width += glyph.advance;
        }
        width
    })
}
