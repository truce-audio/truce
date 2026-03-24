//! Headless GPU rendering for snapshot testing.
//!
//! Renders a `BuiltinEditor` through the GPU backend (same pipeline as
//! the DAW) and returns raw RGBA pixels. Used by `truce-test` for
//! snapshot comparisons.

use std::sync::Arc;

use truce_gui::editor::BuiltinEditor;
use truce_gui::layout::GridLayout;
use truce_params::Params;

use crate::backend::WgpuBackend;

/// Render a built-in GUI to RGBA pixels via headless GPU.
///
/// Returns `(pixels, width, height)` where pixels is RGBA row-major.
pub fn render_to_pixels<P: Params + 'static>(
    params: Arc<P>,
    layout: GridLayout,
) -> (Vec<u8>, u32, u32) {
    let w = layout.width;
    let h = layout.height;

    let mut editor = BuiltinEditor::new_grid(params, layout);
    let mut backend = WgpuBackend::headless(w, h, 1.0)
        .expect("Failed to create headless GPU backend for snapshot");

    editor.render_to(&mut backend);
    let pixels = backend.read_pixels();
    (pixels, w, h)
}
