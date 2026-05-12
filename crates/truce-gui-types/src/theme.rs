/// Color as RGBA (0.0–1.0).
#[derive(Clone, Copy, Debug)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    #[must_use]
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    #[must_use]
    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }
}

// `Color::to_skia` / `Color::to_premultiplied` (tiny-skia
// conversions) live in `truce-gui` — they're rasterizer-specific
// helpers, and putting them here would pull tiny-skia into the
// light crate. Backends that need them call the conversion at the
// raster boundary instead.

/// Visual theme for the built-in GUI.
#[derive(Clone, Debug)]
pub struct Theme {
    pub background: Color,
    pub surface: Color,
    pub primary: Color,
    pub accent: Color,
    pub text: Color,
    pub text_dim: Color,
    pub knob_track: Color,
    pub knob_fill: Color,
    pub knob_pointer: Color,
    pub header_bg: Color,
    pub header_text: Color,
}

impl Theme {
    #[must_use]
    pub fn dark() -> Self {
        Self {
            background: Color::rgb(0.12, 0.12, 0.14),
            surface: Color::rgb(0.18, 0.18, 0.22),
            primary: Color::rgb(0.30, 0.60, 0.95),
            accent: Color::rgb(0.45, 0.45, 0.45),
            text: Color::rgb(0.90, 0.90, 0.92),
            text_dim: Color::rgb(0.55, 0.55, 0.60),
            knob_track: Color::rgb(0.25, 0.25, 0.30),
            knob_fill: Color::rgb(0.30, 0.60, 0.95),
            knob_pointer: Color::rgb(0.95, 0.95, 0.97),
            header_bg: Color::rgb(0.08, 0.08, 0.10),
            header_text: Color::rgb(0.75, 0.75, 0.80),
        }
    }
}
