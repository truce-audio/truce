//! Theme bridge between truce-gui colors and iced Theme.

use iced::Color;

/// Create the default truce dark theme for iced.
pub fn truce_dark_theme() -> iced::Theme {
    iced::Theme::custom(
        "Truce Dark".to_string(),
        iced::theme::Palette {
            background: Color::from_rgb(0.12, 0.12, 0.14),
            text: Color::from_rgb(0.90, 0.90, 0.92),
            primary: Color::from_rgb(0.30, 0.60, 0.95),
            success: Color::from_rgb(0.30, 0.80, 0.30),
            danger: Color::from_rgb(0.90, 0.30, 0.30),
        },
    )
}

// Widget-specific colors matching truce-gui's dark theme.
pub const KNOB_TRACK: Color = Color {
    r: 0.25,
    g: 0.25,
    b: 0.30,
    a: 1.0,
};
pub const KNOB_FILL: Color = Color {
    r: 0.30,
    g: 0.60,
    b: 0.95,
    a: 1.0,
};
pub const KNOB_POINTER: Color = Color {
    r: 0.95,
    g: 0.95,
    b: 0.97,
    a: 1.0,
};
pub const TEXT_DIM: Color = Color {
    r: 0.55,
    g: 0.55,
    b: 0.60,
    a: 1.0,
};
pub const SURFACE: Color = Color {
    r: 0.18,
    g: 0.18,
    b: 0.22,
    a: 1.0,
};
pub const ACCENT: Color = Color {
    r: 0.45,
    g: 0.45,
    b: 0.45,
    a: 1.0,
};
pub const HEADER_BG: Color = Color {
    r: 0.08,
    g: 0.08,
    b: 0.10,
    a: 1.0,
};
pub const METER_CLIP: Color = Color {
    r: 0.90,
    g: 0.30,
    b: 0.30,
    a: 1.0,
};
