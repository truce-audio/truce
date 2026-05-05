//! Build-script helper for truce plugins with a Slint GUI.
//!
//! Wraps [`slint_build::compile_with_config`] and pre-fills the
//! truce-specific bits — the `@truce` widget library path and the
//! include path that lets `.slint` files do `import
//! "JetBrainsMono-Regular.ttf";`.
//!
//! The plugin author's `build.rs` becomes:
//!
//! ```rust,ignore
//! fn main() {
//!     truce_slint_build::compile("ui/main.slint").unwrap();
//! }
//! ```
//!
//! Bundling: the widget `.slint` files and the `JetBrains Mono` ttf
//! ride along inside this crate (see the `include = […]` list in
//! `Cargo.toml`). At the consuming crate's build time we
//! materialize them into `OUT_DIR` and hand those paths to
//! `slint-build`. That removes the historical "needs a local truce
//! checkout" requirement — a plugin published to crates.io that
//! depends on `truce-slint-build` from the registry builds without
//! any out-of-band file paths.
//!
//! Re-running on font edits is wired up via
//! `cargo:rerun-if-changed`, so updating the bundled ttf in this
//! crate triggers a rebuild of every dependent.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Filename inside `OUT_DIR/fonts/` that the bundled `JetBrains Mono`
/// is written to. Plugin `.slint` files import the font by this
/// exact name (`import "JetBrainsMono-Regular.ttf";`), so the
/// constant is the contract.
const FONT_FILENAME: &str = "JetBrainsMono-Regular.ttf";

/// `@truce` library import name. Plugin `.slint` files write
/// `import { Knob } from "@truce";`; this is the half slint-build
/// resolves against the widget library entry point.
const LIBRARY_NAME: &str = "truce";

// --- Bundled assets -------------------------------------------------------

/// Bundled `.slint` widget library, keyed by destination filename
/// inside `OUT_DIR/ui/`. `widgets.slint` is the entry point the
/// `@truce` library import resolves to; the rest are pulled in via
/// relative `import` statements from `widgets.slint`.
const WIDGET_SOURCES: &[(&str, &str)] = &[
    ("widgets.slint", include_str!("../ui/widgets.slint")),
    ("knob.slint", include_str!("../ui/knob.slint")),
    ("meter.slint", include_str!("../ui/meter.slint")),
    ("selector.slint", include_str!("../ui/selector.slint")),
    ("slider.slint", include_str!("../ui/slider.slint")),
    ("toggle.slint", include_str!("../ui/toggle.slint")),
    ("xy_pad.slint", include_str!("../ui/xy_pad.slint")),
];

const FONT_BYTES: &[u8] = include_bytes!("../fonts/JetBrainsMono-Regular.ttf");

// --- Public API -----------------------------------------------------------

/// Compile `slint_entry` (relative to `CARGO_MANIFEST_DIR` of the
/// caller, same as [`slint_build::compile`]) using the truce widget
/// library and font include path.
///
/// # Errors
///
/// Returns [`CompileError`] if `OUT_DIR` is missing or unwritable,
/// the bundled assets fail to materialize, or `slint-build` itself
/// rejects the input `.slint` file.
pub fn compile(slint_entry: impl AsRef<Path>) -> Result<(), CompileError> {
    let out_dir = std::env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .ok_or(CompileError::NoOutDir)?;

    let ui_dir = materialize_widgets(&out_dir)?;
    let font_dir = materialize_font(&out_dir)?;

    let widgets_entry = ui_dir.join("widgets.slint");
    let mut library_paths = std::collections::HashMap::new();
    library_paths.insert(LIBRARY_NAME.to_string(), widgets_entry);

    let config = slint_build::CompilerConfiguration::new()
        .with_library_paths(library_paths)
        .with_include_paths(vec![font_dir]);

    slint_build::compile_with_config(slint_entry, config)
        .map_err(|e| CompileError::Slint(format!("{e}")))?;

    Ok(())
}

/// Errors surfaced by [`compile`]. The `Slint` variant carries the
/// stringified `slint-build` error rather than re-exporting that
/// crate's error type, so a future `slint-build` major bump doesn't
/// force a major bump here.
#[derive(Debug)]
pub enum CompileError {
    /// `OUT_DIR` env var wasn't set. Cargo always sets it for build
    /// scripts; this fires only if `compile` is called from a
    /// non-build-script context.
    NoOutDir,
    /// Filesystem failure while writing bundled assets to `OUT_DIR`.
    Io(std::io::Error),
    /// `slint-build` rejected the input `.slint` file (parse error,
    /// type error, missing component, etc.).
    Slint(String),
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoOutDir => {
                f.write_str("OUT_DIR not set — call truce_slint_build::compile from a build script")
            }
            Self::Io(e) => write!(f, "writing bundled assets to OUT_DIR: {e}"),
            Self::Slint(e) => write!(f, "slint compile failed: {e}"),
        }
    }
}

impl Error for CompileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CompileError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// --- Internals ------------------------------------------------------------

fn materialize_widgets(out_dir: &Path) -> Result<PathBuf, CompileError> {
    let ui_dir = out_dir.join("truce-slint-build/ui");
    fs::create_dir_all(&ui_dir)?;
    for (name, source) in WIDGET_SOURCES {
        write_if_changed(&ui_dir.join(name), source.as_bytes())?;
    }
    Ok(ui_dir)
}

fn materialize_font(out_dir: &Path) -> Result<PathBuf, CompileError> {
    let font_dir = out_dir.join("truce-slint-build/fonts");
    fs::create_dir_all(&font_dir)?;
    write_if_changed(&font_dir.join(FONT_FILENAME), FONT_BYTES)?;
    Ok(font_dir)
}

/// Write `bytes` to `path` only if the destination is missing or
/// has a different content hash. Avoids touching the file's mtime
/// on every build, which would force `slint-build` (and any
/// downstream `cargo:rerun-if-changed`-aware step) to re-run
/// unnecessarily.
fn write_if_changed(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Ok(existing) = fs::read(path)
        && existing == bytes
    {
        return Ok(());
    }
    fs::write(path, bytes)
}
