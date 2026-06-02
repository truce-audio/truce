//! Headless vizia screenshot rendering for tests.
//!
//! Drives vizia's `BackendContext::draw` against a CPU-backed Skia
//! raster surface so `Editor::screenshot()` works without an active
//! OS window / GL context. The interactive editor path
//! (`vizia_baseview`) uses a GPU-backed Skia surface wrapped around
//! an OpenGL framebuffer; vizia's draw pipeline itself is
//! surface-agnostic, so swapping in a raster surface gives the same
//! pixels minus the GL upload.

use skia_safe::{AlphaType, ColorType, ImageInfo, surfaces};
use truce_core::editor::PluginContext;
use truce_params::Params;
use vizia::backend::{BackendContext, WindowDescription};
use vizia::context::WindowState;
use vizia::prelude::*;

use crate::editor::SetupFn;
use crate::param_lens::ParamLens;

/// Empty `View` mounted at `Entity::root()` to satisfy the assumption
/// in `vizia_core` that the root entity carries a window-typed view -
/// the live backends mount their windowing-backend's `WindowView`
/// here. We never receive events on the headless surface, so the
/// view stays empty.
struct HeadlessRoot;
impl View for HeadlessRoot {}

pub(crate) fn render_with_state<P: Params + 'static>(
    setup: &SetupFn<P>,
    typed_ctx: PluginContext<P>,
    size: (u32, u32),
    stylesheets: &[&'static str],
    font: Option<&'static [u8]>,
) -> Option<(Vec<u8>, u32, u32)> {
    // Headless renders pin DPI to 1.0: `EditorScale` only matters when
    // there's a host-supplied factor, and screenshot baselines stay
    // reproducible across Retina / non-Retina dev machines this way.
    let dpi_factor: f32 = 1.0;
    let (logical_w, logical_h) = size;
    let phys_w = logical_w;
    let phys_h = logical_h;

    // Match `vizia_baseview::ViziaWindow::new`: fresh `Context`, install
    // default classes, wrap in `BackendContext` for the window-level
    // hooks, then `add_main_window` + a root `View`. Without the root
    // `View` the draw pass walks past `Entity::root()` looking for a
    // view-bearing entity and renders nothing.
    let mut cx = Context::new();
    cx.add_built_in_styles();
    let mut backend = BackendContext::new(cx);

    let mut win_desc = WindowDescription::new();
    win_desc.inner_size = vizia::prelude::WindowSize {
        width: logical_w,
        height: logical_h,
    };
    backend.add_main_window(Entity::root(), &win_desc, dpi_factor);
    backend.add_window(HeadlessRoot);
    // `draw_system` early-returns if the `windows` map is empty
    // (vizia_core/src/systems/draw.rs:9). The live `vizia_baseview`
    // backend inserts the same `WindowState` here; without it the
    // draw pass produces a clear-to-white frame and quits.
    backend.context().windows.insert(
        Entity::root(),
        WindowState {
            window_description: win_desc.clone(),
            ..Default::default()
        },
    );

    {
        let inner = backend.context();
        if let Some(bytes) = font {
            inner.add_font_mem(bytes);
        }
        // Match `ViziaEditor::open`: every user-supplied stylesheet
        // goes in, in the order it was added via `with_stylesheet`.
        // No CSS is force-applied by truce-vizia itself.
        for css in stylesheets {
            let _ = inner.add_stylesheet(*css);
        }
        // The plugin author's setup closure mounts widgets onto the
        // root entity exactly as it does in the live editor.
        let lens = ParamLens::new(typed_ctx);
        (setup)(inner, lens.clone());
        // No event loop runs in the screenshot path, so the editor's
        // timer never fires. Push current meter values into the
        // signals once so the rendered bars reflect the live store
        // value rather than the on-build snapshot.
        lens.refresh_meters();
    }

    // Order mirrors `ApplicationRunner::on_frame`: tree -> style ->
    // (skip animations - they're time-based) -> visual updates. The
    // `set_window_size` re-affirms the root viewport after tree
    // construction so layout has a non-zero bounds to fit into.
    // `needs_refresh` populates the redraw list + restyle/relayout
    // flags so the first draw covers every entity instead of the
    // empty initial set.
    backend.set_window_size(Entity::root(), u32_to_f32(phys_w), u32_to_f32(phys_h));
    backend.needs_refresh(Entity::root());
    backend.process_tree_updates();
    backend.process_style_updates();
    backend.process_visual_updates();

    // CPU-side Skia surface. `n32_premul` picks the platform-native
    // 32-bit format (BGRA8 on macOS / Windows skia, RGBA8 on Linux
    // skia); we read back through an explicit `RGBA_8888` `ImageInfo`
    // so the byte layout handed to the caller is RGBA regardless.
    let width_signed = u32_to_i32(phys_w);
    let height_signed = u32_to_i32(phys_h);
    let mut surface = surfaces::raster_n32_premul((width_signed, height_signed))?;
    let mut dirty_surface =
        surface.new_surface_with_dimensions((width_signed.max(1), height_signed.max(1)))?;
    backend.draw(Entity::root(), &mut surface, &mut dirty_surface);

    let rgba_info = ImageInfo::new(
        (width_signed, height_signed),
        ColorType::RGBA8888,
        AlphaType::Premul,
        None,
    );
    let row_bytes = (phys_w as usize) * 4;
    let mut pixels = vec![0u8; row_bytes * phys_h as usize];
    if !surface.read_pixels(&rgba_info, &mut pixels, row_bytes, (0, 0)) {
        return None;
    }

    Some((pixels, phys_w, phys_h))
}

// `f32`'s 23-bit mantissa caps at ~16M; window dimensions stay well
// below that. The cast carries the lint suppression so the call site
// reads cleanly.
#[allow(clippy::cast_precision_loss)]
fn u32_to_f32(v: u32) -> f32 {
    v as f32
}

// Window dimensions cap at 2^15-ish in any practical UI; the
// `u32 -> i32` cast can't wrap inside that domain. Suppressing the
// lint here keeps the read-back paths above unannotated.
#[allow(clippy::cast_possible_wrap)]
fn u32_to_i32(v: u32) -> i32 {
    v as i32
}
