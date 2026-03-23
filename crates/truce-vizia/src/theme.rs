//! Default dark theme matching truce-gui's visual style.

use vizia::prelude::*;

const DEFAULT_THEME: &str = include_str!("default_theme.css");

/// Apply the default truce dark theme to a vizia context.
pub fn apply_default_theme(cx: &mut Context) {
    cx.add_stylesheet(DEFAULT_THEME)
        .expect("failed to load truce-vizia default theme");
}
