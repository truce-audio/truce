//! Font rendering using fontdue (TrueType rasterization).
//!
//! Embeds JetBrains Mono at compile time. Rasterizes glyphs on first
//! use and caches them for subsequent draws.

use std::collections::HashMap;
use std::sync::Mutex;

pub static FONT_DATA: &[u8] = include_bytes!("../fonts/JetBrainsMono-Regular.ttf");

/// Cached rasterized glyph.
struct CachedGlyph {
    bitmap: Vec<u8>,   // alpha values, row-major
    width: u32,
    height: u32,
    advance: f32,      // horizontal advance in pixels
    y_offset: f32,     // offset from baseline (negative = above baseline)
}

/// Global glyph cache. Keyed by (character, size_tenths).
/// size_tenths = (size * 10.0) as u32 to avoid float keys.
static CACHE: Mutex<Option<GlyphCache>> = Mutex::new(None);

struct GlyphCache {
    font: fontdue::Font,
    glyphs: HashMap<(char, u32), CachedGlyph>,
}

fn get_or_init_cache() -> std::sync::MutexGuard<'static, Option<GlyphCache>> {
    let mut guard = CACHE.lock().unwrap();
    if guard.is_none() {
        let font = fontdue::Font::from_bytes(
            FONT_DATA,
            fontdue::FontSettings::default(),
        ).expect("failed to parse embedded font");
        *guard = Some(GlyphCache {
            font,
            glyphs: HashMap::new(),
        });
    }
    guard
}

fn size_key(size: f32) -> u32 {
    (size * 10.0) as u32
}

/// Rasterize and cache a glyph, returning its cached data.
fn get_glyph(cache: &mut GlyphCache, ch: char, size: f32) -> &CachedGlyph {
    let key = (ch, size_key(size));
    if !cache.glyphs.contains_key(&key) {
        let (metrics, bitmap) = cache.font.rasterize(ch, size);
        cache.glyphs.insert(key, CachedGlyph {
            bitmap,
            width: metrics.width as u32,
            height: metrics.height as u32,
            advance: metrics.advance_width,
            y_offset: metrics.ymin as f32,
        });
    }
    cache.glyphs.get(&key).unwrap()
}

/// Draw text into an RGBA premultiplied pixel buffer.
///
/// This is the main entry point for font rendering. It handles glyph
/// caching internally — first call for a given (char, size) pair
/// rasterizes; subsequent calls blit from cache.
pub fn draw_text_fontdue(
    pixmap_data: &mut [u8],
    pixmap_width: u32,
    pixmap_height: u32,
    text: &str,
    x: f32,
    y: f32,
    size: f32,
    r: f32, g: f32, b: f32, a: f32,
) {
    let mut guard = get_or_init_cache();
    let cache = guard.as_mut().unwrap();

    let mut cursor_x = x;

    // Get line metrics for vertical positioning.
    let line_metrics = cache.font.horizontal_line_metrics(size);
    let ascent = line_metrics.map(|m| m.ascent).unwrap_or(size * 0.8);

    for ch in text.chars() {
        let glyph = get_glyph(cache, ch, size);
        let gw = glyph.width;
        let gh = glyph.height;

        // Position: cursor_x for horizontal, baseline + y_offset for vertical
        let gx = cursor_x as i32;
        let gy = (y + ascent - glyph.y_offset - gh as f32) as i32;

        // Blit glyph bitmap (alpha values) into the RGBA pixmap
        let cr = (r * 255.0) as u8;
        let cg = (g * 255.0) as u8;
        let cb = (b * 255.0) as u8;

        for row in 0..gh {
            for col in 0..gw {
                let px = gx + col as i32;
                let py = gy + row as i32;

                if px < 0 || py < 0 || px >= pixmap_width as i32 || py >= pixmap_height as i32 {
                    continue;
                }

                let alpha = glyph.bitmap[(row * gw + col) as usize];
                if alpha == 0 { continue; }

                let ga = (alpha as f32 / 255.0) * a;
                let idx = ((py as u32 * pixmap_width + px as u32) * 4) as usize;
                if idx + 3 >= pixmap_data.len() { continue; }

                // Alpha blending (premultiplied)
                let src_r = (cr as f32 * ga) as u8;
                let src_g = (cg as f32 * ga) as u8;
                let src_b = (cb as f32 * ga) as u8;
                let src_a = (ga * 255.0) as u8;

                let inv_sa = 1.0 - ga;

                pixmap_data[idx]     = (src_r as f32 + pixmap_data[idx] as f32 * inv_sa) as u8;
                pixmap_data[idx + 1] = (src_g as f32 + pixmap_data[idx + 1] as f32 * inv_sa) as u8;
                pixmap_data[idx + 2] = (src_b as f32 + pixmap_data[idx + 2] as f32 * inv_sa) as u8;
                pixmap_data[idx + 3] = (src_a as f32 + pixmap_data[idx + 3] as f32 * inv_sa) as u8;
            }
        }

        cursor_x += glyph.advance;
    }
}

/// Measure text width in pixels.
pub fn text_width_fontdue(text: &str, size: f32) -> f32 {
    let mut guard = get_or_init_cache();
    let cache = guard.as_mut().unwrap();

    let mut width = 0.0f32;
    for ch in text.chars() {
        let glyph = get_glyph(cache, ch, size);
        width += glyph.advance;
    }
    width
}
