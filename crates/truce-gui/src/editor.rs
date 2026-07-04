//! Built-in editor using the CPU render backend.
//!
//! Renders parameter widgets via `RenderBackend`. Uses tiny-skia for
//! software rasterization and baseview + wgpu for window management
//! and blitting. For GPU-accelerated rendering see the `truce-gpu`
//! crate which provides `GpuEditor` wrapping this editor.

#[cfg(feature = "cpu")]
use std::ptr;
use std::sync::Arc;
#[cfg(feature = "cpu")]
use std::sync::Mutex;
#[cfg(feature = "cpu")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};

use truce_core::Float;
#[cfg(feature = "cpu")]
use truce_core::editor::RawWindowHandle;
#[cfg(feature = "cpu")]
use truce_core::editor::{Editor, ResizeCorrector};
use truce_core::editor::{PluginContext, PluginContextReadF32};
use truce_params::Params;

#[cfg(feature = "cpu")]
use crate::backend_cpu::CpuBackend;
use crate::interaction::{self, InputEvent, InteractionState, ParamEdit};
use crate::layout::{GridLayout, Layout, PluginLayout};
#[cfg(feature = "cpu")]
use crate::platform::EditorScale;
use crate::render::RenderBackend;
use crate::render_core::{
    EditorSnapshotClosures, build_snapshot_closures as build_snapshot_closures_impl,
    render_widgets as render_widgets_impl,
};
use crate::theme::Theme;
use crate::widgets;

/// Built-in editor that renders parameter widgets to a pixel buffer.
///
/// Uses the CPU backend (tiny-skia) for software rasterization. When
/// `open()` is called, creates a baseview window and blits pixels via wgpu.
pub struct BuiltinEditor<P: Params> {
    params: Arc<P>,
    layout: Layout,
    theme: Theme,
    /// CPU pixmap rendering target. Only present when the `cpu`
    /// feature is on; in `gpu`-only mode `BuiltinEditor` is wrapped
    /// by `GpuEditor`, which renders through `WgpuBackend` directly
    /// via [`Self::render_to`] without touching this field.
    #[cfg(feature = "cpu")]
    backend: Option<CpuBackend>,
    interaction: InteractionState,
    context: Option<PluginContext>,
    /// Active baseview window handle for the cpu-path `Editor`
    /// impl. Only meaningful when `cpu` is on.
    #[cfg(feature = "cpu")]
    window: Option<baseview::WindowHandle>,
    /// Weak-ish handle to the blit backend the window-handler
    /// materializes. The editor keeps the canonical `Arc` and the
    /// handler gets a clone. On close we take the `Option` out of
    /// the inner mutex - dropping the wgpu Surface synchronously -
    /// before asking baseview to tear the `NSView` down.
    #[cfg(feature = "cpu")]
    blit_backend: Option<SharedBackend>,
    /// Set whenever something visible changes (param edited via the
    /// UI, host-driven state reload, explicit `request_repaint` by
    /// plugin code). `on_frame` clears it and only does the
    /// rasterize + blit pass when it was true.
    ///
    /// Shared so `PluginContext::set_param` and `state_changed`
    /// closures can flip it without touching editor internals.
    needs_repaint: Arc<AtomicBool>,
    /// Normalized values captured at the last render pass, in the
    /// same order as `interaction.knob_regions`. Used to detect
    /// host-driven param changes (automation, preset recall) - if any
    /// live value drifts from the last-painted one, we force a
    /// repaint even if the UI never received a direct edit. Only
    /// the cpu path's incremental render uses this signal.
    #[cfg(feature = "cpu")]
    last_painted_values: Vec<f32>,
    /// Live content-scale factor (a [`crate::platform::EditorScale`]).
    /// `set_scale_factor` (host) writes the cell; the baseview
    /// handler holds a clone, compares against `last_applied_scale`
    /// each frame, and rebuilds the CPU pixmap + reconfigures the
    /// wgpu surface when the value diverges. Only consumed by the
    /// cpu path; in gpu-only mode `GpuEditor` has its own
    /// `EditorScale` and this field is unused.
    #[cfg(feature = "cpu")]
    scale: EditorScale,
    /// Standalone hosts set this (via `set_uses_system_scale`) so the
    /// editor honors the desktop `Xft.dpi` scale on Linux; plugins leave
    /// it false and drive scale from the host instead. See
    /// [`crate::platform::editor_window_scale`]. No effect off Linux.
    #[cfg(feature = "cpu")]
    use_system_scale: bool,
    /// Whether the host announced a content scale via `set_scale_factor`.
    /// On Linux this gates whether an embedded editor trusts `scale`
    /// (host-announced) or defaults to 1.0.
    #[cfg(feature = "cpu")]
    host_scale_set: bool,
    /// Meter IDs referenced by the layout, collected once at
    /// construction. Meters are display-only values written from the
    /// audio thread (`PluginContext::get_meter`); they never move
    /// through the param system, so the CPU repaint gate needs to poll
    /// them explicitly to know when to redraw. Empty for layouts with
    /// no meters - the poll then short-circuits.
    #[cfg(feature = "cpu")]
    meter_ids: Vec<u32>,
    /// Meter values captured at the last repaint, parallel to
    /// `meter_ids`. `detect_meter_changes` compares the live values
    /// against these to flip the dirty bit only when a meter actually
    /// moved (the gpu path repaints unconditionally and ignores this).
    #[cfg(feature = "cpu")]
    last_meter_values: Vec<f32>,
    /// Host-driven resize handoff. `Editor::set_size` snaps the
    /// requested width to a whole number of `cell_size + gap`
    /// steps, reflows the grid via `GridLayout::refit_cols`, and
    /// packs the resulting `(w, h)` here. `on_frame` drains the
    /// cell at the top of each tick and applies the size to the
    /// CPU pixmap, wgpu surface, interaction regions, and the
    /// baseview window itself - same handoff shape the egui / iced
    /// / slint editors use. `0` is the "no pending resize"
    /// sentinel; an unchanged editor pays one atomic load per
    /// frame.
    #[cfg(feature = "cpu")]
    pending_size: Arc<AtomicU64>,
}

// SAFETY: `baseview::WindowHandle` holds a raw native window pointer
// (HWND / NSView / X11 Window) and is not auto-`Send`. Hosts call
// `Editor::open` / `idle` / `close` from a single dedicated GUI thread
// - never concurrently and never from the audio thread - so the
// handle is only ever touched on the thread that created it. The
// `Editor` trait requires `Send` so the editor can live behind a
// trait object; this impl asserts that the type doesn't escape its
// thread in practice. All other fields (`Arc<P>`, `Layout`, `Theme`,
// `Option<CpuBackend>`, etc.) are themselves `Send`.
unsafe impl<P: Params> Send for BuiltinEditor<P> {}

/// Gather every meter ID referenced by a layout, in layout order. The
/// CPU editor polls these each frame to decide when a meter moved and
/// the surface needs a repaint.
#[cfg(feature = "cpu")]
fn collect_meter_ids(layout: &Layout) -> Vec<u32> {
    let mut ids = Vec::new();
    match layout {
        Layout::Rows(pl) => {
            for row in &pl.rows {
                for knob in &row.knobs {
                    if let Some(m) = &knob.meter_ids {
                        ids.extend_from_slice(m);
                    }
                }
            }
        }
        Layout::Grid(gl) => {
            for widget in &gl.widgets {
                if let Some(m) = &widget.meter_ids {
                    ids.extend_from_slice(m);
                }
            }
        }
    }
    ids
}

impl<P: Params + 'static> BuiltinEditor<P> {
    /// Request a repaint on the next idle tick. Call this if plugin
    /// code mutates display state outside the normal param or
    /// `state_changed` pathways (uncommon). User interaction and
    /// host automation already flag themselves dirty automatically.
    pub fn request_repaint(&self) {
        self.needs_repaint.store(true, Ordering::Release);
    }

    /// Only consumed by the cpu Editor impl's render gate.
    #[cfg(feature = "cpu")]
    fn take_needs_repaint(&self) -> bool {
        self.needs_repaint.swap(false, Ordering::AcqRel)
    }

    /// Compare the values just read by `update_interaction` (live from
    /// the host / params Arc) against those captured at the last
    /// render. A mismatch means an automation lane wrote a new value,
    /// a preset was recalled, or some other off-UI state change
    /// happened - force a repaint so the widget tracks it.
    ///
    /// Only used by the cpu blit path's incremental render gate;
    /// the gpu path repaints every frame and skips this check.
    #[cfg(feature = "cpu")]
    fn detect_host_param_changes(&mut self) {
        let regions = &self.interaction.knob_regions;
        if regions.len() != self.last_painted_values.len() {
            // Region set changed (e.g. after a layout rebuild). Force
            // a repaint and re-sync on the next paint.
            self.request_repaint();
            return;
        }
        for (i, region) in regions.iter().enumerate() {
            if (region.normalized_value - self.last_painted_values[i]).abs() > f32::EPSILON {
                self.request_repaint();
                return;
            }
        }
    }

    /// Snapshot the regions' normalized values for the next frame's
    /// automation detection. Called after each render. Only used by
    /// the cpu blit path.
    #[cfg(feature = "cpu")]
    fn stash_painted_values(&mut self) {
        let regions = &self.interaction.knob_regions;
        // Resize-then-overwrite reuses the existing allocation
        // unchanged when the region count is steady (the common
        // case - knob layouts only change on
        // `interaction.build_regions`). The previous
        // clear-then-extend form pumped through the iterator path
        // every frame even when the length didn't change.
        self.last_painted_values.resize(regions.len(), 0.0);
        for (slot, region) in self.last_painted_values.iter_mut().zip(regions.iter()) {
            *slot = region.normalized_value;
        }
    }

    /// Poll the layout's meters and flag a repaint when any value
    /// moved since the last frame. Meters are display-only values the
    /// audio thread reports through `PluginContext::get_meter`; they
    /// don't flow through `detect_host_param_changes` (which only
    /// inspects knob param regions), so without this the CPU gate would
    /// freeze the meter until an unrelated repaint trigger (a knob drag,
    /// host param churn) happened to fire. The gpu path repaints every
    /// frame and skips this entirely.
    #[cfg(feature = "cpu")]
    #[allow(clippy::float_cmp)]
    fn detect_meter_changes(&mut self) {
        if self.meter_ids.is_empty() {
            return;
        }
        let Some(ctx) = self.context.as_ref() else {
            return;
        };
        let current: Vec<f32> = self.meter_ids.iter().map(|&id| ctx.get_meter(id)).collect();
        if current != self.last_meter_values {
            self.last_meter_values = current;
            self.request_repaint();
        }
    }

    pub fn new(params: Arc<P>, layout: PluginLayout) -> Self {
        Self::with_layout_inner(params, Layout::Rows(layout))
    }

    pub fn new_with_layout(params: Arc<P>, layout: Layout) -> Self {
        Self::with_layout_inner(params, layout)
    }

    pub fn new_grid(params: Arc<P>, layout: GridLayout) -> Self {
        Self::with_layout_inner(params, Layout::Grid(layout))
    }

    fn with_layout_inner(params: Arc<P>, layout: Layout) -> Self {
        #[cfg(feature = "cpu")]
        let meter_ids = collect_meter_ids(&layout);
        Self {
            params,
            layout,
            theme: Theme::dark(),
            #[cfg(feature = "cpu")]
            backend: None,
            interaction: InteractionState::default(),
            context: None,
            #[cfg(feature = "cpu")]
            window: None,
            #[cfg(feature = "cpu")]
            blit_backend: None,
            needs_repaint: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "cpu")]
            last_painted_values: Vec::new(),
            #[cfg(feature = "cpu")]
            scale: EditorScale::new(crate::backing_scale()),
            #[cfg(feature = "cpu")]
            use_system_scale: false,
            #[cfg(feature = "cpu")]
            host_scale_set: false,
            #[cfg(feature = "cpu")]
            meter_ids,
            #[cfg(feature = "cpu")]
            last_meter_values: Vec::new(),
            #[cfg(feature = "cpu")]
            pending_size: Arc::new(AtomicU64::new(0)),
        }
    }

    #[must_use]
    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Render the full UI to the internal CPU pixel buffer.
    ///
    /// Only available when the `cpu` feature is on. In `gpu`-only
    /// mode, render through [`Self::render_to`] with a
    /// `truce_gpu::WgpuBackend` instead.
    ///
    /// # Panics
    ///
    /// Panics if the lazy `CpuBackend::new` allocation fails (out of
    /// memory or zero dimensions). The backend is allocated on first
    /// render - subsequent calls reuse it.
    #[cfg(feature = "cpu")]
    pub fn render(&mut self) {
        let (w, h) = (self.layout.width(), self.layout.height());
        let scale = self.scale.get_f32();
        let owned = self.build_snapshot_closures();
        let snapshot = owned.as_snapshot();
        // `Pixmap::new` returns `None` for zero / unrepresentable
        // physical dimensions, which can happen when a host probes
        // `gui_get_size` against an unreasonable scale or when an
        // edge-case `set_size` makes it through with extreme
        // values. Previously this site unwrapped, which turned a
        // recoverable rendering miss into a Rust panic that the
        // VST3 `extern "C"` boundary couldn't catch - Cubase then
        // hit it as an uncaught exception and aborted. Skip the
        // frame instead; the next `on_frame` tick will retry once
        // dimensions settle.
        let backend = if let Some(ref mut b) = self.backend {
            b
        } else {
            let Some(b) = CpuBackend::new(w, h, scale) else {
                log::warn!("CpuBackend allocation failed for {w}x{h} @ {scale}x; skipping frame");
                return;
            };
            self.backend.insert(b)
        };
        render_widgets_impl(
            &self.layout,
            &self.theme,
            &mut self.interaction,
            &snapshot,
            backend,
        );
    }

    /// Build owned boxed closures from `self.context` / `self.params` that
    /// back a `ParamSnapshot`. Each closure clones the `Arc<P>` or the
    /// `PluginContext`, so `EditorSnapshotClosures` is `'static` and safe
    /// to hold across a borrow of `&mut self.interaction`. Delegates to
    /// the shared `render_core` impl so the iOS editor doesn't have to
    /// duplicate the (~100-line) closure scaffolding.
    fn build_snapshot_closures(&self) -> EditorSnapshotClosures {
        build_snapshot_closures_impl(&self.params, self.context.as_ref())
    }

    /// Apply a single `ParamEdit` returned by `interaction::dispatch`.
    fn apply_edit(&self, edit: ParamEdit) {
        match edit {
            ParamEdit::Begin { id } => {
                if let Some(ref ctx) = self.context {
                    ctx.begin_edit(id);
                }
            }
            ParamEdit::Set { id, normalized } => {
                self.params.set_normalized(id, f64::from(normalized));
                if let Some(ref ctx) = self.context {
                    ctx.set_param(id, f64::from(normalized));
                }
                self.request_repaint();
            }
            ParamEdit::End { id } => {
                if let Some(ref ctx) = self.context {
                    ctx.end_edit(id);
                }
            }
        }
    }

    /// Feed a batch of input events through `interaction::dispatch` and
    /// apply the resulting param edits. Flags a repaint when hover,
    /// dropdown-open state, or any param moved.
    ///
    /// Typically callers build the events by running each baseview
    /// event through [`interaction::BaseviewTranslator`] and batching
    /// the non-`None` results.
    pub fn dispatch_events(&mut self, events: &[InputEvent]) {
        let hover_before = self.interaction.hover_idx;
        let dd_before = self.interaction.dropdown_is_open();
        let owned = self.build_snapshot_closures();
        let snapshot = owned.as_snapshot();
        let edits = interaction::dispatch(events, &self.layout, &snapshot, &mut self.interaction);
        let had_edits = !edits.is_empty();
        for e in edits {
            self.apply_edit(e);
        }
        // Anything that changes a pixel on screen flips the dirty
        // bit: param edits (already covered by `apply_edit`), hover
        // highlights moving between widgets, dropdown open/close
        // transitions, and any event that explicitly requested a
        // repaint (e.g. MouseLeave clearing hover state).
        let explicit = self.interaction.take_repaint_request();
        if had_edits
            || explicit
            || self.interaction.hover_idx != hover_before
            || self.interaction.dropdown_is_open() != dd_before
        {
            self.request_repaint();
        }
    }

    /// Get the raw pixel data after rendering (RGBA premultiplied).
    /// Only available when the `cpu` feature is on.
    #[cfg(feature = "cpu")]
    #[must_use]
    pub fn pixel_data(&self) -> Option<&[u8]> {
        self.backend
            .as_ref()
            .map(super::backend_cpu::CpuBackend::data)
    }

    // --- Public API for external backends (truce-gpu) ---

    /// Whether the editor has an active context.
    #[must_use]
    pub fn has_context(&self) -> bool {
        self.context.is_some()
    }

    /// Take the editor context, leaving `None` in its place.
    /// Used by hot-reload to preserve the context when swapping editors.
    pub fn take_context(&mut self) -> Option<PluginContext> {
        self.context.take()
    }

    /// Set the editor context (host callbacks) without opening the CPU view.
    pub fn set_context(&mut self, context: PluginContext) {
        self.context = Some(context);
        match &self.layout {
            Layout::Rows(pl) => self.interaction.build_regions(pl),
            Layout::Grid(gl) => self.interaction.build_regions_grid(gl),
        }
    }

    /// Editor logical size (width, height in points). Inherent
    /// method so it stays callable when the `Editor` trait impl is
    /// cfg'd out in gpu-only builds.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.layout.width(), self.layout.height())
    }

    /// Whether the editor supports host/user-driven resize. Inherent
    /// for the same reason as [`Self::size`]: the GPU editor wraps this
    /// type and delegates to it in gpu-only builds where the `Editor`
    /// trait impl is cfg'd out.
    #[must_use]
    pub fn can_resize(&self) -> bool {
        match &self.layout {
            Layout::Grid(gl) => gl.resizable,
            // `PluginLayout` (the older row-based layout) doesn't have a
            // reflow path yet; pin it until that lands.
            Layout::Rows(_) => false,
        }
    }

    /// Minimum logical size in points. Inherent (see [`Self::size`]).
    #[must_use]
    pub fn min_size(&self) -> (u32, u32) {
        match &self.layout {
            // A non-resizable grid has exactly one size. The snapped
            // probes only span a range for resizable grids (which set
            // explicit min/max cells); probing a fixed grid reflows it
            // and can report min > max, so pin both to the natural size
            // like a `Rows` layout.
            Layout::Grid(gl) if gl.resizable => gl.min_snapped_size(),
            Layout::Grid(_) | Layout::Rows(_) => self.size(),
        }
    }

    /// Maximum logical size in points. Inherent (see [`Self::size`]).
    #[must_use]
    pub fn max_size(&self) -> (u32, u32) {
        match &self.layout {
            Layout::Grid(gl) if gl.resizable => gl.max_snapped_size(),
            Layout::Grid(_) | Layout::Rows(_) => self.size(),
        }
    }

    /// Cell-step resize increment, or `None` when not resizable.
    /// Inherent (see [`Self::size`]).
    #[must_use]
    pub fn size_increment(&self) -> Option<(u32, u32)> {
        match &self.layout {
            // Both axes snap on the same cell step. Only resizable
            // grids advertise it; `Rows` layouts are pinned.
            Layout::Grid(gl) if gl.resizable => {
                let step = gl.resize_step();
                Some((step, step))
            }
            _ => None,
        }
    }

    /// Whether the standalone host may maximize the window. Inherent
    /// (see [`Self::size`]) so the gpu-only `GpuEditor` wrapper can
    /// reach it when this `Editor` impl is cfg'd out. Sourced from the
    /// grid's `.maximizable()` (default `false`); `Rows` layouts are
    /// fixed-size and never maximizable, and the value is moot there
    /// anyway since `can_resize` is `false`.
    #[must_use]
    pub fn can_maximize(&self) -> bool {
        match &self.layout {
            Layout::Grid(gl) => gl.maximizable,
            Layout::Rows(_) => false,
        }
    }

    /// Snap a requested logical size to whole cells, reflow the grid,
    /// and post the result for the next frame. Returns `true` when
    /// accepted. Inherent (see [`Self::size`]).
    pub fn set_size(&mut self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 || !self.can_resize() {
            return false;
        }
        let Layout::Grid(ref mut gl) = self.layout else {
            return false;
        };
        // Snap each axis to a whole cell step independently:
        // width drives the column count (auto-flow widgets reflow,
        // explicit widgets stay put), height drives the row count
        // (purely a bookkeeping value `compute_size` uses to grow
        // the grid past the bottommost widget with empty trailing
        // space). The wider snap *then* the taller snap so the
        // final cached `(width, height)` includes both axes.
        gl.refit_cols(width);
        let (new_w, new_h) = gl.refit_rows(height);
        // The CPU backend's `BuiltinWindowHandler` reads `pending_size`
        // to drive its surface/window resize. The GPU wrapper instead
        // polls `size()` each frame, so the cell only exists (and only
        // needs writing) in cpu builds; the reflow above is the part
        // both paths share.
        #[cfg(feature = "cpu")]
        self.pending_size.store(
            (u64::from(new_w) << 32) | u64::from(new_h),
            Ordering::Release,
        );
        #[cfg(not(feature = "cpu"))]
        let _ = (new_w, new_h);
        // Flip the dirty bit so a quiescent editor (no automation,
        // no UI edits) still wakes up the `on_frame` repaint gate
        // and picks up the new size on the next tick.
        self.request_repaint();
        true
    }

    /// Notify the widget tree that plugin state was restored
    /// (preset recall, undo, session load). Inherent for the same
    /// reason as [`Self::size`] above.
    pub fn state_changed(&mut self) {
        self.request_repaint();
    }

    /// Render all widgets to an external `RenderBackend`.
    ///
    /// Used by `truce-gpu` to draw through the GPU backend instead of
    /// the internal CPU backend.
    pub fn render_to(&mut self, backend: &mut dyn RenderBackend) {
        update_interaction(self);
        let owned = self.build_snapshot_closures();
        let snapshot = owned.as_snapshot();
        render_widgets_impl(
            &self.layout,
            &self.theme,
            &mut self.interaction,
            &snapshot,
            backend,
        );
    }
}

/// Test-only ergonomic wrappers. Production callers go through
/// `dispatch_events` (usually with events synthesized by
/// [`crate::interaction::BaseviewTranslator`]).
#[cfg(test)]
impl<P: Params + 'static> BuiltinEditor<P> {
    fn on_mouse_down(&mut self, x: f32, y: f32) {
        self.dispatch_events(&[InputEvent::MouseDown {
            pointer_id: truce_gui_types::interaction::SINGLE_POINTER,
            x,
            y,
            button: crate::interaction::MouseButton::Left,
        }]);
    }

    fn on_mouse_up(&mut self, x: f32, y: f32) {
        self.dispatch_events(&[InputEvent::MouseUp {
            pointer_id: truce_gui_types::interaction::SINGLE_POINTER,
            x,
            y,
            button: crate::interaction::MouseButton::Left,
        }]);
    }

    fn on_mouse_moved(&mut self, x: f32, y: f32) {
        self.dispatch_events(&[InputEvent::MouseMove {
            pointer_id: truce_gui_types::interaction::SINGLE_POINTER,
            x,
            y,
        }]);
    }
}

// ---------------------------------------------------------------------------
// C callbacks - thin wrappers that cast the context pointer back to &mut Self
// ---------------------------------------------------------------------------

/// Update interaction regions and live param values.
///
/// Takes `&mut BuiltinEditor<P>` so the borrow checker enforces
/// non-aliasing - the function only touches Rust references and is
/// fully safe.
pub fn update_interaction<P: Params + 'static>(editor: &mut BuiltinEditor<P>) {
    match &editor.layout {
        Layout::Rows(pl) => {
            editor.interaction.build_regions(pl);
            let mut flat_idx = 0usize;
            for row in &pl.rows {
                for knob_def in &row.knobs {
                    if let Some(region) = editor.interaction.knob_regions.get_mut(flat_idx) {
                        region.widget_type = resolve_widget_type(
                            knob_def.widget,
                            knob_def.param_id,
                            &*editor.params,
                        );
                    }
                    flat_idx += 1;
                }
            }
        }
        Layout::Grid(gl) => {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type =
                        resolve_widget_type(gw.widget, gw.param_id, &*editor.params);
                }
            }
        }
    }
    for region in &mut editor.interaction.knob_regions {
        if let Some(ref ctx) = editor.context {
            // Resolves through `PluginContextReadF32` - bridge's `f64` narrows inside.
            region.normalized_value = ctx.get_param(region.param_id);
        } else {
            region.normalized_value =
                f32::from_f64(editor.params.get_normalized(region.param_id).unwrap_or(0.0));
        }
    }
}

// ---------------------------------------------------------------------------
// Baseview WindowHandler - drives the CPU render loop
// ---------------------------------------------------------------------------
//
// On macOS + AAX: blits via CoreGraphics (CGImage → CALayer) to avoid Metal
// autorelease crashes with multiple editor windows.
// Otherwise: blits via wgpu fullscreen triangle.
//
// The whole section (window handler + Editor trait impl below) is
// gated behind the `cpu` feature. In `gpu`-only mode the editor is
// provided by `GpuEditor` (which wraps `BuiltinEditor::render_to`
// through `truce_gpu::WgpuBackend`) and these wgpu-blit details
// drop out of the compile.

/// Build the blit backend around a surface pump (see
/// `truce_gpu::pump`). GPU init runs on the pump - off the host's GUI
/// thread on Windows, where a stalled driver used to freeze the DAW
/// at editor open - and [`BlitParts`] is adopted lazily via
/// [`BlitBackend::parts_mut`]. Returns `None` when the pump can't
/// spawn at all (blank but harmless editor).
#[cfg(feature = "cpu")]
fn create_wgpu_backend(
    window: &mut baseview::Window,
    phys_w: u32,
    phys_h: u32,
) -> Option<BlitBackend> {
    // The panic flag is unused (no device-loss rebuild in this
    // handler); a dead pump just leaves the editor blank.
    let device_lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pump = unsafe {
        truce_gpu::pump::SurfacePump::spawn(
            window,
            &device_lost,
            Box::new(move |_, adapter, surface| {
                // `downlevel_defaults` caps `max_texture_dimension_2d` at 2048
                // - on Retina (2x), that means the editor can't physically exceed
                // 1024 logical points per axis before `surface.configure` panics
                // with a validation error. Use the adapter's actual limits so a
                // resizable layout (e.g. the GUI zoo) can grow to its declared
                // `max_cols` / `max_rows` envelope without tripping the cap, then
                // belt-and-braces clamp resize requests in `BlitBackend::resize`.
                let adapter_limits = adapter.limits();
                let max_texture_dim = adapter_limits.max_texture_dimension_2d;
                let (device, queue) =
                    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                        label: Some("truce-gui"),
                        required_features: wgpu::Features::empty(),
                        required_limits: adapter_limits,
                        experimental_features: wgpu::ExperimentalFeatures::default(),
                        memory_hints: wgpu::MemoryHints::Performance,
                        trace: wgpu::Trace::Off,
                    }))
                    .ok()?;

                let caps = surface.get_capabilities(adapter);
                let format = caps
                    .formats
                    .iter()
                    .find(|f| f.is_srgb())
                    .copied()
                    .unwrap_or(caps.formats[0]);

                // Same belt-and-braces clamp as `BlitBackend::resize` applies on
                // subsequent reconfigures: a host could open the editor at a
                // logical * DPI size that already exceeds `max_texture_dim`
                // (e.g. a fixed-size editor on a 3x display whose physical
                // dimensions are over the device cap).
                let init_w = phys_w.clamp(1, max_texture_dim);
                let init_h = phys_h.clamp(1, max_texture_dim);
                let surface_config = wgpu::SurfaceConfiguration {
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    format,
                    width: init_w,
                    height: init_h,
                    // Windows: a Fifo (AutoVsync) present blocks when the
                    // child-window swapchain backs up, freezing the host
                    // (REAPER) when it lands on the GUI thread and risking
                    // a GPU-watchdog (TDR) hang. Non-blocking present
                    // there; elsewhere keeps vsync.
                    #[cfg(target_os = "windows")]
                    present_mode: wgpu::PresentMode::AutoNoVsync,
                    #[cfg(not(target_os = "windows"))]
                    present_mode: wgpu::PresentMode::AutoVsync,
                    desired_maximum_frame_latency: 2,
                    alpha_mode: wgpu::CompositeAlphaMode::Auto,
                    view_formats: vec![],
                };

                // Blit texture matches the CPU pixmap, which is sized at
                // physical pixels (see CpuBackend's scale handling). With texture
                // and surface at the same physical size, the full-screen-triangle
                // blit samples 1:1 - no stretch, no Retina blur.
                let blit = crate::blit::BlitPipeline::new(&device, format, init_w, init_h);

                let parts = BlitParts {
                    blit,
                    surface_config: surface_config.clone(),
                    queue,
                    device: device.clone(),
                    max_texture_dim,
                };
                Some((parts, device, surface_config))
            }),
        )
    }?;
    Some(BlitBackend {
        client: pump.client(),
        parts: None,
        pump,
    })
}

/// The pump's init product: everything the GUI thread needs to encode
/// the blit. Field-declaration order doubles as the implicit drop
/// order - children before parent: per-pipeline GPU resources, then
/// queue, then device. (The surface itself lives with the pump and
/// drops with it.)
#[cfg(feature = "cpu")]
struct BlitParts {
    blit: crate::blit::BlitPipeline,
    /// Local bookkeeping copy; the authoritative configure happens on
    /// the pump.
    surface_config: wgpu::SurfaceConfiguration,
    queue: wgpu::Queue,
    device: wgpu::Device,
    /// Adapter-reported `max_texture_dimension_2d`. `resize` clamps
    /// each axis against this before reconfiguring so a host- or
    /// DPI-driven resize past the device's texture cap can't trip a
    /// wgpu validation panic (which unwinds out of the editor on the
    /// host's UI thread and aborts the standalone / the DAW).
    max_texture_dim: u32,
}

/// The blit pipeline plus the surface pump that owns its swapchain.
/// `parts` stays `None` until the pump finishes GPU init (immediately
/// on macOS / Linux, where init runs inline).
#[cfg(feature = "cpu")]
struct BlitBackend {
    client: truce_gpu::pump::PumpClient,
    parts: Option<BlitParts>,
    pump: truce_gpu::pump::SurfacePump<BlitParts>,
}

#[cfg(feature = "cpu")]
impl BlitBackend {
    /// The pump's init product, adopting it if it just landed. `None`
    /// while GPU init is still running (or after it failed).
    fn parts_mut(&mut self) -> Option<&mut BlitParts> {
        if self.parts.is_none()
            && let Some(parts) = self.pump.take_init()
        {
            self.parts = Some(parts);
        }
        self.parts.as_mut()
    }

    /// Reconfigure the wgpu surface and blit texture for a new physical
    /// size. Used when `Editor::set_scale_factor` reports a host-driven
    /// DPI change - the logical editor size doesn't change, but the
    /// physical pixmap and surface need to grow / shrink to match.
    fn resize(&mut self, phys_w: u32, phys_h: u32) {
        let client = self.client.clone();
        let Some(parts) = self.parts_mut() else {
            return;
        };
        let phys_w = phys_w.clamp(1, parts.max_texture_dim);
        let phys_h = phys_h.clamp(1, parts.max_texture_dim);
        parts.surface_config.width = phys_w;
        parts.surface_config.height = phys_h;
        client.resize(phys_w, phys_h);
        parts.blit.resize(&parts.device, phys_w, phys_h);
    }

    /// Reconfigure only the swapchain surface to a new physical size,
    /// leaving the blit texture (the CPU pixmap source) untouched.
    ///
    /// The surface must track the window's *real* physical extent so it
    /// always covers it. That extent is set by the WM (X11, now
    /// cell-snapped via resize-increment hints) or the host, and is not
    /// bit-identical to `to_physical_px(logical, scale)` - sizing the
    /// surface from the logical value instead leaves the window's
    /// trailing edge showing whatever is behind it. The blit's
    /// fullscreen-triangle pass samples its texture across the whole
    /// surface, so surface != texture size just rescales the image to
    /// fill - no gap. Called from the `Resized` handler, where the
    /// window's actual physical size is authoritative.
    fn configure_surface(&mut self, phys_w: u32, phys_h: u32) {
        let client = self.client.clone();
        let Some(parts) = self.parts_mut() else {
            return;
        };
        let phys_w = phys_w.clamp(1, parts.max_texture_dim);
        let phys_h = phys_h.clamp(1, parts.max_texture_dim);
        if parts.surface_config.width == phys_w && parts.surface_config.height == phys_h {
            return;
        }
        parts.surface_config.width = phys_w;
        parts.surface_config.height = phys_h;
        client.resize(phys_w, phys_h);
    }
}

/// Shared ownership of the blit backend between `BuiltinEditor` and the
/// `BuiltinWindowHandler` baseview hands us. Sharing lets the editor
/// drop the wgpu surface *before* it asks baseview to close the
/// `NSView`. Important on AAX where interleaving Metal teardown with
/// baseview's close sequence inside Pro Tools' outer autorelease pool
/// leaves stale refs in DFW container views.
#[cfg(feature = "cpu")]
type SharedBackend = Arc<Mutex<Option<BlitBackend>>>;

#[cfg(feature = "cpu")]
struct BuiltinWindowHandler<P: Params> {
    /// Raw pointer to the `BuiltinEditor` owned by the host. Valid only
    /// while `backend.lock()` returns `Some(_)`. `BuiltinEditor::close`
    /// takes the inner `Option<BlitBackend>` (atomically through this
    /// mutex) before returning, and the host can only drop the editor
    /// after `close()` returns - so any frame that holds the lock and
    /// finds the inner option `Some` is guaranteed the editor is still
    /// alive. The lock acquire is the synchronization point that keeps
    /// an in-flight `on_frame` from dereferencing this pointer after
    /// the host dropped the editor while baseview's render thread still
    /// had a callback queued. Only accessed from the GUI thread.
    editor: *mut BuiltinEditor<P>,
    backend: SharedBackend,
    /// Canonical baseview → `InputEvent` translator. Handles cursor
    /// tracking, double-click synthesis, and line→pixel scroll
    /// conversion once for everyone.
    translator: crate::interaction::BaseviewTranslator,
    /// Paces paints to the compositor's measured consumption rate so
    /// per-tick repaints (meters) can't park the host's GUI thread in
    /// the swapchain acquire - see [`crate::PaintPacer`].
    pacer: crate::platform::PaintPacer,
    /// Last scale we built the CPU pixmap + wgpu surface against.
    /// `on_frame` reads `editor.scale.get()` (via the raw ptr deref
    /// it already does) and compares; on divergence it rebuilds the
    /// pixmap and reconfigures the surface. Unlike egui / iced /
    /// slint we don't need a separate `EditorScale` clone on the
    /// handler - the editor is reachable through the same ptr that
    /// guards the lifecycle, so reading `editor.scale` is the
    /// canonical access path.
    last_applied_scale: f32,
    /// Enforces min/max/aspect on host resizes that bypassed the
    /// format's negotiation hooks (Linux hosts resizing the embed
    /// window directly).
    resize_corrector: ResizeCorrector,
}

// SAFETY: The raw pointer is only accessed from the GUI thread.
// baseview requires Send for WindowHandler.
#[cfg(feature = "cpu")]
unsafe impl<P: Params> Send for BuiltinWindowHandler<P> {}

#[cfg(feature = "cpu")]
impl<P: Params + 'static> BuiltinWindowHandler<P> {
    fn on_frame_inner(&mut self, window: &mut baseview::Window) {
        // Lock the shared backend cell *before* deref'ing `self.editor`.
        // `BuiltinEditor::close` calls `drop(guard.take())` on the same
        // mutex before returning; the host then drops the editor. So
        // either we observe `Some(_)` here (close hasn't taken it yet,
        // editor still alive) or we observe `None` and return without
        // touching `self.editor`. Either way the deref below is sound.
        let Ok(mut guard) = self.backend.lock() else {
            return;
        };
        if guard.is_none() {
            // Editor already dropped the backend in its close path.
            // Nothing to do - baseview will tear us down next.
            return;
        }

        let editor = unsafe { &mut *self.editor };

        // Pick up host-driven `set_size` requests posted to the
        // shared `pending_size` cell since the last frame. The
        // editor's `set_size` has already snapped to a whole
        // column count and reflowed the grid via
        // `GridLayout::refit_cols`; here we rebuild the CPU pixmap
        // at the new logical size, reconfigure the wgpu blit
        // surface to the new physical extent, refresh the
        // interaction-region cache against the post-reflow widget
        // layout, and resize the baseview window itself so the
        // host's outer container follows. Same handoff pattern the
        // egui / iced / slint editors use.
        let pending = editor.pending_size.swap(0, Ordering::Acquire);
        if pending != 0 {
            #[allow(clippy::cast_possible_truncation)]
            let new_w = (pending >> 32) as u32;
            #[allow(clippy::cast_possible_truncation)]
            let new_h = (pending & 0xFFFF_FFFF) as u32;
            if new_w > 0 && new_h > 0 {
                let scale = editor.scale.get();
                let scale_f32 = editor.scale.get_f32();
                let phys_w = crate::platform::to_physical_px(new_w, scale);
                let phys_h = crate::platform::to_physical_px(new_h, scale);
                editor.backend = CpuBackend::new(new_w, new_h, scale_f32);
                if let Some(backend) = guard.as_mut() {
                    backend.resize(phys_w, phys_h);
                }
                match &editor.layout {
                    Layout::Rows(pl) => editor.interaction.build_regions(pl),
                    Layout::Grid(gl) => editor.interaction.build_regions_grid(gl),
                }
                window.resize(baseview::Size::new(f64::from(new_w), f64::from(new_h)));
                editor.request_repaint();
            }
        }

        // Re-anchor on every frame so any host-driven drift of the
        // child `NSView`'s origin gets corrected before the next
        // paint. The wrapper installs `MinYMargin | MaxXMargin`
        // (via `anchor_child_to_top`) on the child, which keeps the
        // child top-anchored across *parent-driven* resizes - but
        // both the editor resizing itself (via `window.resize`
        // above) and the host reseating the child via its own
        // `setFrameOrigin:` call (REAPER's plug-in framework does
        // this) bypass AppKit's autoresize math. The result is a
        // child whose top edge drifts off the host pane and the
        // editor's GAIN header / knob row clip above the visible
        // area while the canvas's empty trailing space + bottom
        // labels show inside. Running every frame is cheap - it's
        // one Cocoa frame query and a no-op short-circuit when
        // already anchored - and is the cleanest place to assert
        // the invariant the wrapper expects.
        // Skip the whole frame while the editor isn't presentable:
        // detached / occluded on macOS, host child window hidden /
        // minimized on Windows (no-op on Linux).
        {
            use raw_window_handle::HasRawWindowHandle;
            if crate::platform::should_skip_frame(window.raw_window_handle()) {
                return;
            }
        }
        #[cfg(target_os = "macos")]
        {
            use raw_window_handle::HasRawWindowHandle;
            crate::platform::reanchor_to_superview_top(window.raw_window_handle());
        }

        // Pick up scale changes that landed in the shared cell since
        // the last frame - either from a host callback (CLAP
        // `set_scale`, VST3 `IPlugViewContentScaleSupport`) or from
        // the OS-driven `Resized` path writing through `info.scale()`.
        // Logical w×h is fixed when resize is disallowed; only the
        // logical→physical ratio moves through here.
        if let Some(cur_scale) = editor.scale.take_change(&mut self.last_applied_scale) {
            let (lw, lh) = editor.size();
            let phys_w = crate::platform::to_physical_px(lw, f64::from(cur_scale));
            let phys_h = crate::platform::to_physical_px(lh, f64::from(cur_scale));
            editor.backend = CpuBackend::new(lw, lh, cur_scale);
            if let Some(backend) = guard.as_mut() {
                backend.resize(phys_w, phys_h);
            }
            editor.request_repaint();
        }

        update_interaction(editor);
        // Pick up host automation / preset recall that changed params
        // without going through the UI: flips the dirty bit so the
        // normal gate below still has the chance to short-circuit when
        // truly nothing moved.
        editor.detect_host_param_changes();
        editor.detect_meter_changes();
        // Compositor pacing veto - before `take_needs_repaint` so the
        // dirty bit survives the held ticks and the deferred paint
        // still happens. Windows skips the veto: the pump pre-acquires
        // frames off-thread and `try_take_frame` returning `None`
        // already paces paints to the compositor, so holding here only
        // adds latency.
        if cfg!(not(target_os = "windows")) && self.pacer.should_hold() {
            return;
        }
        if !editor.take_needs_repaint() {
            return;
        }
        // Get the pump's frame BEFORE rasterizing or uploading. During
        // resize churn no frame is available (the pump is busy
        // reconfiguring); skipping everything here saves the wasted
        // CPU raster and keeps queue work (texture upload, submit) off
        // the GUI thread while the pump's configure is in flight -
        // those contend on wgpu's internal locks. On Windows the take
        // never blocks (pump pre-acquires); elsewhere it acquires
        // inline with the usual stale-surface recovery.
        let client = {
            let backend = guard
                .as_mut()
                .expect("guard was checked Some above and the lock is still held");
            if backend.parts_mut().is_none() {
                // GPU init still pending on the pump (Windows) or
                // failed; re-arm the dirty bit so the first ready
                // frame paints instead of waiting for the next edit.
                editor.request_repaint();
                return;
            }
            backend.client.clone()
        };
        let frame = client.try_take_frame();
        self.pacer.record_acquire(client.last_acquire_wait());
        let Some(frame) = frame else {
            // Windows: the pump is still acquiring - re-arm the
            // dirty bit so the paint lands when the frame is
            // ready. Elsewhere `None` is a transient Timeout /
            // Occluded; skip and let the next edit repaint.
            #[cfg(target_os = "windows")]
            editor.request_repaint();
            return;
        };
        editor.render();
        editor.stash_painted_values();

        if let Some(pixels) = editor.pixel_data() {
            let backend = guard
                .as_mut()
                .expect("guard was checked Some above and the lock is still held");
            let Some(parts) = backend.parts_mut() else {
                client.discard(frame);
                editor.request_repaint();
                return;
            };
            let BlitParts {
                device,
                queue,
                surface_config,
                blit,
                ..
            } = parts;
            // A resize raced the acquire: the frame is at the old
            // extent; discard it (the pump reconfigures + reacquires).
            if (frame.texture.width(), frame.texture.height())
                != (surface_config.width, surface_config.height)
            {
                client.discard(frame);
                editor.request_repaint();
                return;
            }
            blit.update(queue, pixels);
            let view = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder =
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            blit.render(
                queue,
                &mut encoder,
                &view,
                surface_config.width,
                surface_config.height,
            );
            queue.submit(std::iter::once(encoder.finish()));
            client.present(frame);
        } else {
            client.discard(frame);
        }
    }

    // Mirrors the by-value `WindowHandler::on_event` signature it's
    // called from; pedantic clippy can't tell that the `match event`
    // arms only bind `Copy` fields.
    #[allow(clippy::needless_pass_by_value)]
    fn on_event_inner(
        &mut self,
        window: &mut baseview::Window,
        event: baseview::Event,
    ) -> baseview::EventStatus {
        // `window` is only read on Windows (focus-on-click below);
        // discard explicitly on other platforms so the lint stays quiet.
        #[cfg(not(target_os = "windows"))]
        let _ = &window;

        if let baseview::Event::Mouse(baseview::MouseEvent::ButtonPressed {
            button: baseview::MouseButton::Left,
            ..
        }) = &event
        {
            // WS_CHILD plugin windows don't receive WM_KEYDOWN
            // until focused; baseview doesn't SetFocus on click,
            // so we do it here. Without this, text-edit widgets
            // never see keystrokes (the DAW keeps eating them for
            // transport shortcuts).
            #[cfg(target_os = "windows")]
            {
                if !window.has_focus() {
                    window.focus();
                }
            }
        }

        // Lock-then-check-then-deref pattern, same as `on_frame` -
        // the backend cell is the synchronization point with
        // `BuiltinEditor::close`. If the cell is `None`, the editor
        // pointer is no longer guaranteed valid and we must not deref.
        let Ok(mut guard) = self.backend.lock() else {
            return baseview::EventStatus::Ignored;
        };
        if guard.is_none() {
            return baseview::EventStatus::Ignored;
        }

        match event {
            baseview::Event::Mouse(_) => {
                let Some(input) = self.translator.translate(&event) else {
                    return baseview::EventStatus::Ignored;
                };
                let editor = unsafe { &mut *self.editor };
                editor.dispatch_events(&[input]);
                baseview::EventStatus::Captured
            }
            baseview::Event::Window(baseview::WindowEvent::Resized(info)) => {
                // Two things can flow through `Resized`:
                //  - A backing-scale change (monitor-boundary drag,
                //    host calling `set_scale_factor`): logical w×h is
                //    invariant, only `info.scale()` matters.
                //  - A logical resize via the autoresize cascade
                //    (host grows the parent NSView with our child
                //    tagged `WidthSizable | HeightSizable`, or the
                //    standalone window grows around us). For
                //    resizable editors we route the new bounds into
                //    `set_size` so the grid reflows; fixed-size
                //    editors stay pinned.
                let editor = unsafe { &mut *self.editor };
                editor.scale.set(info.scale());
                crate::platform::note_linux_scale_factor(info.scale());
                let phys = info.physical_size();
                if editor.can_resize() {
                    let scale = info.scale();
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let (lw, lh) = if scale > 0.0 {
                        (
                            (f64::from(phys.width) / scale).round() as u32,
                            (f64::from(phys.height) / scale).round() as u32,
                        )
                    } else {
                        (phys.width, phys.height)
                    };
                    if lw > 0 && lh > 0 {
                        // A host that resized the embed window directly
                        // never ran the format's constraint preflight -
                        // fit here and push the corrected size back.
                        let ((fw, fh), correct) = self.resize_corrector.fit(
                            lw,
                            lh,
                            editor.min_size(),
                            editor.max_size(),
                            editor.aspect_ratio(),
                        );
                        if (fw, fh) != editor.size() {
                            editor.set_size(fw, fh);
                        }
                        if let Some((rw, rh)) = correct {
                            // On Linux, hosts that bypass size negotiation
                            // (Bitwig) ignore this request and react by
                            // *growing* the embed window - a resize loop.
                            // Clamp the content (and counter-resize our child)
                            // but never ask the host to resize its frame.
                            // mac/windows honor it (and negotiate via
                            // `checkSizeConstraint`) anyway.
                            #[cfg(not(target_os = "linux"))]
                            if let Some(ctx) = editor.context.as_ref() {
                                let _ = ctx.request_resize(rw, rh);
                            }
                            #[cfg(target_os = "linux")]
                            let _ = (rw, rh);
                        }
                    }
                }
                // Keep the swapchain covering the window's *actual*
                // physical size. The WM (X11 resize-increment snap) or
                // host sets that size, and it isn't bit-identical to the
                // `to_physical_px(logical)` the `on_frame` resize paths
                // configure the surface to - so without this the trailing
                // edge of the window shows whatever is behind it. Driving
                // the surface from the authoritative `info.physical_size()`
                // here closes that gap; the blit scales the pixmap to fill.
                if phys.width > 0
                    && phys.height > 0
                    && let Some(backend) = guard.as_mut()
                {
                    backend.configure_surface(phys.width, phys.height);
                }
                // Always repaint on a `Resized`, even when the logical
                // size is unchanged. Our own `set_size` -> `on_frame`
                // resize is asynchronous on X11: `on_frame` reconfigures
                // the surface and presents one frame *before* the
                // `ConfigureNotify` actually grows the child window, then
                // clears the dirty bit. The trailing `Resized` that
                // reports the now-grown window carries a logical size
                // that already matches `editor.size()`, so without this
                // the gate short-circuits and the freshly exposed region
                // is never painted - it shows whatever was behind the
                // window until the next unrelated repaint.
                editor.request_repaint();
                baseview::EventStatus::Ignored
            }
            _ => baseview::EventStatus::Ignored,
        }
    }
}

#[cfg(feature = "cpu")]
impl<P: Params + 'static> baseview::WindowHandler for BuiltinWindowHandler<P> {
    fn on_frame(&mut self, window: &mut baseview::Window) {
        // Catch panics at the FFI boundary. baseview calls us through
        // an `extern "C-unwind"` AppKit override; an unwinding Rust
        // panic becomes an ObjC exception and `NSApplication run`
        // rethrows it, terminating the host. Swallow the panic and
        // log it so the host stays alive.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.on_frame_inner(window);
        }));
        if let Err(e) = result {
            let msg = if let Some(s) = e.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            log::error!("BuiltinWindowHandler::on_frame panic swallowed: {msg}");
        }
    }

    fn on_event(
        &mut self,
        window: &mut baseview::Window,
        event: baseview::Event,
    ) -> baseview::EventStatus {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.on_event_inner(window, event)
        }));
        result.unwrap_or_else(|e| {
            let msg = if let Some(s) = e.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            log::error!("BuiltinWindowHandler::on_event panic swallowed: {msg}");
            baseview::EventStatus::Ignored
        })
    }
}

// ---------------------------------------------------------------------------
// Editor trait implementation
// ---------------------------------------------------------------------------

/// Resolve widget type: explicit override > auto-detect from param range.
fn resolve_widget_type<P: Params>(
    widget: Option<crate::layout::WidgetKind>,
    param_id: u32,
    params: &P,
) -> widgets::WidgetType {
    match widget {
        Some(crate::layout::WidgetKind::Knob) => widgets::WidgetType::Knob,
        Some(crate::layout::WidgetKind::Slider) => widgets::WidgetType::Slider,
        Some(crate::layout::WidgetKind::Toggle) => widgets::WidgetType::Toggle,
        Some(crate::layout::WidgetKind::Dropdown) => widgets::WidgetType::Dropdown,
        Some(crate::layout::WidgetKind::Meter) => widgets::WidgetType::Meter,
        Some(crate::layout::WidgetKind::XYPad) => widgets::WidgetType::XYPad,
        None => {
            let param_info = params
                .param_infos()
                .iter()
                .find(|i| i.id == param_id)
                .copied();
            match param_info.as_ref().map(|i| &i.range) {
                Some(truce_params::ParamRange::Discrete { min: 0, max: 1 }) => {
                    widgets::WidgetType::Toggle
                }
                Some(truce_params::ParamRange::Enum { .. }) => widgets::WidgetType::Dropdown,
                _ => widgets::WidgetType::Knob,
            }
        }
    }
}

#[cfg(feature = "cpu")]
impl<P: Params + 'static> Editor for BuiltinEditor<P> {
    fn size(&self) -> (u32, u32) {
        (self.layout.width(), self.layout.height())
    }

    fn state_changed(&mut self) {
        // Preset recall / undo / session load: params moved without
        // going through the UI, so force the next idle tick to repaint.
        self.request_repaint();
    }

    // These forward to the inherent methods of the same name (inherent
    // methods win method resolution, so `self.foo()` is not recursive).
    // The logic lives inherently so the gpu-only `GpuEditor` wrapper can
    // reach it when this `Editor` impl is cfg'd out.
    fn can_resize(&self) -> bool {
        self.can_resize()
    }

    fn can_maximize(&self) -> bool {
        self.can_maximize()
    }

    fn min_size(&self) -> (u32, u32) {
        self.min_size()
    }

    fn max_size(&self) -> (u32, u32) {
        self.max_size()
    }

    fn size_increment(&self) -> Option<(u32, u32)> {
        self.size_increment()
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        self.set_size(width, height)
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        let (w, h) = self.size();
        // Drop any stale `set_size` that fired before this `open()`
        // so the next frame doesn't immediately re-resize the
        // freshly-built window to a previous request.
        self.pending_size.store(0, Ordering::Relaxed);
        // Refresh the shared scale from the parent window - on macOS
        // this is the live `[NSWindow backingScaleFactor]`, on
        // Windows the per-monitor DPI from the parent HWND. Any
        // `set_scale_factor` the host issues after open will overwrite
        // through the same shared cell.
        // Pick the baseview scale policy. On Linux an embedded plugin
        // follows the host's scale (default 1.0) rather than the desktop
        // Xft.dpi, which a non-DPI-aware host (Bitwig) doesn't share; the
        // standalone and every macOS/Windows path keep SystemScaleFactor.
        let scale_policy = if let Some(s) = crate::platform::editor_window_scale(
            self.use_system_scale,
            self.host_scale_set,
            self.scale.get(),
        ) {
            self.scale.set(s);
            baseview::WindowScalePolicy::ScaleFactor(s)
        } else {
            self.scale
                .set(crate::platform::query_backing_scale(&parent));
            baseview::WindowScalePolicy::SystemScaleFactor
        };
        let scale = self.scale.get();
        let scale_f32 = self.scale.get_f32();
        self.backend = CpuBackend::new(w, h, scale_f32);
        self.context = Some(context);

        // Build interaction regions
        match &self.layout {
            Layout::Rows(pl) => self.interaction.build_regions(pl),
            Layout::Grid(gl) => self.interaction.build_regions_grid(gl),
        }

        // Render initial frame and flag dirty so the first `on_frame`
        // blit also runs (the construction default is `false` because a
        // not-yet-opened editor has nothing to paint to).
        self.render();
        self.request_repaint();

        let (lw, lh) = (f64::from(w), f64::from(h));
        let phys_w = crate::platform::to_physical_px(w, scale);
        let phys_h = crate::platform::to_physical_px(h, scale);

        let options = baseview::WindowOpenOptions {
            title: String::from("truce"),
            size: baseview::Size::new(lw, lh),
            scale: scale_policy,
        };

        let parent_wrapper = crate::platform::ParentWindow(parent);
        let editor_addr = ptr::from_mut::<BuiltinEditor<P>>(self) as usize;

        // Shared backend cell: the editor keeps one Arc and baseview's
        // window handler gets the other. At close time the editor
        // takes the inner Option and drops it *before* asking baseview
        // to tear down the NSView.
        let shared_backend: SharedBackend = Arc::new(Mutex::new(None));
        self.blit_backend = Some(shared_backend.clone());
        let shared_for_handler = shared_backend;

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut baseview::Window| {
                let backend = create_wgpu_backend(window, phys_w, phys_h);

                // Render + present an initial frame synchronously, before
                // baseview shows the window. Without this, the window briefly
                // displays whatever garbage is in the surface buffer until the
                // first `on_frame` tick - especially noticeable on VST2
                // (Windows), where `effEditOpen` creates and shows the window
                // in one call. On Windows the pump is still initializing here
                // (`parts_mut` is `None`), so this paint is skipped and the
                // dirty bit set at `open()` covers the first ready frame.
                let editor = unsafe { &mut *(editor_addr as *mut BuiltinEditor<P>) };
                editor.render();
                let mut backend = backend;
                if let Some(pixels) = editor.pixel_data()
                    && let Some(backend) = backend.as_mut()
                {
                    let client = backend.client.clone();
                    if let Some(parts) = backend.parts_mut() {
                        let BlitParts {
                            device,
                            queue,
                            surface_config,
                            blit,
                            ..
                        } = parts;
                        blit.update(queue, pixels);
                        if let Some(frame) = client.try_take_frame() {
                            let view = frame
                                .texture
                                .create_view(&wgpu::TextureViewDescriptor::default());
                            let mut encoder =
                                device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                                    label: None,
                                });
                            blit.render(
                                queue,
                                &mut encoder,
                                &view,
                                surface_config.width,
                                surface_config.height,
                            );
                            queue.submit(std::iter::once(encoder.finish()));
                            client.present(frame);
                        }
                    }
                }

                // Publish the backend into the shared cell. If the
                // editor has already been asked to close (very
                // unlikely race - only if close fires before baseview
                // calls our build closure), the None-check on the
                // mutex side will simply replace Some(None) → Some
                // and everything drops at the usual time.
                if let Ok(mut guard) = shared_for_handler.lock() {
                    *guard = backend;
                }

                BuiltinWindowHandler {
                    editor: editor_addr as *mut BuiltinEditor<P>,
                    backend: shared_for_handler.clone(),
                    translator: crate::interaction::BaseviewTranslator::default(),
                    last_applied_scale: scale_f32,
                    pacer: crate::platform::PaintPacer::default(),
                    resize_corrector: ResizeCorrector::default(),
                }
            },
        );

        self.window = Some(window);
    }

    fn set_scale_factor(&mut self, factor: f64) {
        // Write to the shared cell; the baseview handler picks up the
        // change on its next frame and rebuilds the CPU pixmap +
        // reconfigures the wgpu surface. The trait's default no-op
        // would silently swallow host scale changes here.
        self.host_scale_set = true;
        self.scale.set(factor);
    }

    fn set_uses_system_scale(&mut self, yes: bool) {
        self.use_system_scale = yes;
    }

    fn close(&mut self) {
        // On macOS, wrap the teardown in an autoreleasepool so
        // anything baseview / wgpu / AppKit autoreleases during the
        // view's cleanup drains here rather than escaping into the
        // host's outer pool. AAX / Pro Tools is the canonical host
        // that walks back through residual responders before the
        // pool drains, surfacing use-after-free crashes.
        #[cfg(target_os = "macos")]
        let pool = unsafe {
            unsafe extern "C" {
                fn objc_autoreleasePoolPush() -> *mut std::ffi::c_void;
            }
            objc_autoreleasePoolPush()
        };

        // Drop the wgpu surface (CAMetalLayer, MTLDevice, command
        // queue, etc.) before asking baseview to release the NSView.
        // Keeps the Metal teardown order deterministic. The destructure
        // makes the drop order explicit rather than depending on
        // `BlitPipeline`'s field-declaration order. Order: per-pipeline
        // GPU resources first (textures, bind groups, sampler), then
        // the pump (which owns and releases the surface / swap chain /
        // CAMetalLayer), then queue, then device last - children
        // before parent.
        if let Some(shared) = self.blit_backend.take()
            && let Ok(mut guard) = shared.lock()
            && let Some(backend) = guard.take()
        {
            let BlitBackend {
                client,
                parts,
                pump,
            } = backend;
            if let Some(BlitParts {
                blit,
                surface_config,
                queue,
                device,
                max_texture_dim: _,
            }) = parts
            {
                drop(surface_config);
                drop(blit);
                drop(client);
                drop(pump);
                drop(queue);
                drop(device);
            } else {
                drop(client);
                drop(pump);
            }
        }

        if let Some(mut window) = self.window.take() {
            window.close();
        }
        self.context = None;
        self.backend = None;

        #[cfg(target_os = "macos")]
        unsafe {
            unsafe extern "C" {
                fn objc_autoreleasePoolPop(pool: *mut std::ffi::c_void);
            }
            objc_autoreleasePoolPop(pool);
        }
    }

    fn idle(&mut self) {
        // baseview drives `on_frame` via its internal timer; idle is
        // only meaningful for the headless/standalone case where the
        // caller wants a render cycle to pull pixel data out.
        if self.window.is_none() {
            self.render();
        }
    }

    fn screenshot(
        &mut self,
        _params: Arc<dyn truce_params::Params>,
    ) -> Option<(Vec<u8>, u32, u32)> {
        // Headless render of the widget tree into a fresh
        // `CpuBackend` at the live content scale. Mirrors
        // `GpuEditor::screenshot`'s shape: same `render_to` call
        // path, same physical-size rounding so reference PNGs baked
        // on either backend match dimensions exactly. Used by
        // `truce_test::assert_screenshot::<P>()`.
        let (lw, lh) = self.size();
        let scale = self.scale.get_f32();
        let mut backend = CpuBackend::new(lw, lh, scale)?;
        self.render_to(&mut backend);
        let pixels = backend.data().to_vec();
        let (phys_w, phys_h) = (backend.width(), backend.height());
        Some((pixels, phys_w, phys_h))
    }
}

#[cfg(feature = "cpu")]
impl<P: Params + 'static> Drop for BuiltinEditor<P> {
    fn drop(&mut self) {
        // The baseview `WindowHandle` does not cancel the macOS frame
        // timer when it drops, and the NSView keeps its own strong
        // `Rc<WindowState>`, so the timer keeps firing `on_frame`
        // against the handler's raw `*mut BuiltinEditor`. If the host
        // drops us without calling `Editor::close` first, that pointer
        // dangles the moment our fields (`scale`, the shared backend)
        // are freed - the next tick deref'd freed memory and crashes in
        // `EditorScale::take_change`. Run the same teardown here so the
        // timer is always cancelled before our fields go away; it is
        // idempotent via the `Option::take`s, so a prior `close` makes
        // this a no-op.
        Editor::close(self);
    }
}

#[cfg(test)]
mod tests {
    // Layout-coordinate assertions compare stored anchor values for
    // bit-exact equality (no arithmetic between them).
    #![allow(clippy::float_cmp, clippy::cast_precision_loss)]

    use super::*;
    use crate::layout::{GridLayout, GridWidget, Layout, section, widgets};
    use crate::widgets::WidgetType;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use truce_params::{ParamFlags, ParamInfo, ParamRange, ParamUnit, ParamValueKind, Params};

    // -- Mock Params with one enum param (4 options) and one float --

    struct TestParams {
        values: [AtomicU64; 2],
    }

    impl TestParams {
        fn new() -> Self {
            Self {
                values: [
                    AtomicU64::new(0.0f64.to_bits()),
                    AtomicU64::new(0.0f64.to_bits()),
                ],
            }
        }
    }

    impl truce_params::__private::Sealed for TestParams {}
    impl Params for TestParams {
        fn param_infos(&self) -> Vec<ParamInfo> {
            vec![
                ParamInfo {
                    id: 0,
                    name: "Mode",
                    short_name: "Mode",
                    group: "",
                    range: ParamRange::Enum { count: 4 },
                    default_plain: 0.0,
                    flags: ParamFlags::AUTOMATABLE,
                    unit: ParamUnit::None,
                    kind: ParamValueKind::Enum,
                    midi_map: None,
                    midi_channel: None,
                },
                ParamInfo {
                    id: 1,
                    name: "Gain",
                    short_name: "Gain",
                    group: "",
                    range: ParamRange::Linear { min: 0.0, max: 1.0 },
                    default_plain: 0.5,
                    flags: ParamFlags::AUTOMATABLE,
                    unit: ParamUnit::None,
                    kind: ParamValueKind::Float,
                    midi_map: None,
                    midi_channel: None,
                },
            ]
        }

        fn count(&self) -> usize {
            2
        }

        fn get_normalized(&self, id: u32) -> Option<f64> {
            self.values
                .get(id as usize)
                .map(|v| f64::from_bits(v.load(Ordering::Relaxed)))
        }

        fn set_normalized(&self, id: u32, value: f64) {
            if let Some(v) = self.values.get(id as usize) {
                v.store(value.to_bits(), Ordering::Relaxed);
            }
        }

        fn get_plain(&self, id: u32) -> Option<f64> {
            let norm = self.get_normalized(id)?;
            let info = self.param_infos().iter().find(|i| i.id == id).copied()?;
            Some(info.range.denormalize(norm))
        }

        fn set_plain(&self, id: u32, value: f64) {
            if let Some(info) = self.param_infos().iter().find(|i| i.id == id).copied() {
                self.set_normalized(id, info.range.normalize(value));
            }
        }

        fn format_value(&self, _id: u32, value: f64) -> Option<String> {
            Some(format!("{value:.0}"))
        }

        fn parse_value(&self, _id: u32, _text: &str) -> Option<f64> {
            None
        }
        fn snap_smoothers(&self) {}
        fn set_sample_rate(&self, _: f64) {}

        fn collect_values(&self) -> (Vec<u32>, Vec<f64>) {
            let ids = vec![0, 1];
            let vals: Vec<f64> = ids
                .iter()
                .map(|&id| self.get_plain(id).unwrap_or(0.0))
                .collect();
            (ids, vals)
        }

        fn restore_values(&self, values: &[(u32, f64)]) {
            for &(id, val) in values {
                self.set_plain(id, val);
            }
        }
    }

    impl Default for TestParams {
        fn default() -> Self {
            Self::new()
        }
    }

    // -- Helpers --

    /// Build a `BuiltinEditor` with a dropdown at position 0 and a knob at position 1.
    fn make_editor() -> BuiltinEditor<TestParams> {
        let params = Arc::new(TestParams::new());
        let layout = GridLayout::build(vec![widgets(vec![
            GridWidget::dropdown(0u32, "Mode"),
            GridWidget::knob(1u32, "Gain"),
        ])]);
        let mut editor = BuiltinEditor::new_grid(params, layout);
        // Build interaction regions (normally done in open/render)
        if let Layout::Grid(ref gl) = editor.layout {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type =
                        resolve_widget_type(gw.widget, gw.param_id, &*editor.params);
                }
            }
        }
        // Render once to populate dropdown_anchor_y
        editor.render();
        editor
    }

    /// Build an editor with section breaks to test anchor stability.
    fn make_editor_with_sections() -> BuiltinEditor<TestParams> {
        let params = Arc::new(TestParams::new());
        let layout = GridLayout::build(vec![
            section(
                "SECTION A",
                vec![
                    GridWidget::knob(1u32, "Gain"),
                    GridWidget::knob(1u32, "Gain 2"),
                ],
            ),
            section(
                "SECTION B",
                vec![
                    GridWidget::dropdown(0u32, "Mode"),
                    GridWidget::knob(1u32, "Gain 3"),
                ],
            ),
        ]);
        let mut editor = BuiltinEditor::new_grid(params, layout);
        if let Layout::Grid(ref gl) = editor.layout {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type =
                        resolve_widget_type(gw.widget, gw.param_id, &*editor.params);
                }
            }
        }
        editor.render();
        editor
    }

    /// Find the center of the first dropdown widget's region.
    fn dropdown_center(editor: &BuiltinEditor<TestParams>) -> (f32, f32) {
        let region = editor
            .interaction
            .knob_regions
            .iter()
            .find(|r| r.widget_type == WidgetType::Dropdown)
            .expect("no dropdown in layout");
        (region.x + region.w / 2.0, region.y + region.h / 2.0)
    }

    // -- Tests: dropdown close-on-reclick --

    #[test]
    fn dropdown_click_opens() {
        let mut editor = make_editor();
        let (dx, dy) = dropdown_center(&editor);

        editor.on_mouse_down(dx, dy);
        assert!(editor.interaction.dropdown_is_open());
    }

    #[test]
    fn dropdown_click_toggles_closed() {
        let mut editor = make_editor();
        let (dx, dy) = dropdown_center(&editor);

        // Open
        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);
        assert!(editor.interaction.dropdown_is_open());

        // Click same button again - should close, not reopen
        editor.on_mouse_down(dx, dy);
        assert!(!editor.interaction.dropdown_is_open());
    }

    #[test]
    fn dropdown_click_outside_closes() {
        let mut editor = make_editor();
        let (dx, dy) = dropdown_center(&editor);

        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);
        assert!(editor.interaction.dropdown_is_open());

        // Click far away
        editor.on_mouse_down(0.0, 0.0);
        assert!(!editor.interaction.dropdown_is_open());
    }

    #[test]
    fn dropdown_click_option_selects_and_closes() {
        let mut editor = make_editor();
        let (dx, dy) = dropdown_center(&editor);

        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);
        assert!(editor.interaction.dropdown_is_open());

        // Click the second option (index 1) inside the popup
        let dd = editor.interaction.dropdown.as_ref().unwrap();
        let (px, py, _, _) = dd.popup_rect;
        let item_h = 18.0f32;
        let padding = 4.0f32;
        let option_y = py + padding + item_h + item_h / 2.0; // middle of second item

        // Touch model: down then up at the same point commits the
        // option under the release point. (Down alone starts a
        // popup-drag - the up handler decides commit-vs-scroll.)
        editor.on_mouse_down(px + 10.0, option_y);
        editor.on_mouse_up(px + 10.0, option_y);

        assert!(!editor.interaction.dropdown_is_open());
        // Enum{count:4} → step_count=3 → 4 options. Index 1 → norm = 1/3
        let norm = editor.params.get_normalized(0).unwrap();
        let expected = 1.0 / 3.0;
        assert!(
            (norm - expected).abs() < 0.01,
            "expected {expected:.4}, got {norm}"
        );
    }

    // -- Tests: dropdown anchor positioning --

    #[test]
    fn dropdown_anchor_set_after_render() {
        let editor = make_editor();
        let region = editor
            .interaction
            .knob_regions
            .iter()
            .find(|r| r.widget_type == WidgetType::Dropdown)
            .unwrap();

        // Anchor should be within the widget region (below y, above y+h)
        assert!(
            region.dropdown_anchor_y > region.y,
            "anchor {} should be below region.y {}",
            region.dropdown_anchor_y,
            region.y
        );
        assert!(
            region.dropdown_anchor_y < region.y + region.h,
            "anchor {} should be above region bottom {}",
            region.dropdown_anchor_y,
            region.y + region.h
        );
    }

    #[test]
    fn dropdown_popup_uses_anchor() {
        let mut editor = make_editor();
        let (dx, dy) = dropdown_center(&editor);

        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);

        let dd = editor.interaction.dropdown.as_ref().unwrap();
        let region = &editor.interaction.knob_regions[dd.region_idx];

        // popup_y must equal the stored anchor - popup always
        // anchors directly below the button (scrolls on tight
        // editors rather than relocating).
        assert_eq!(dd.popup_rect.1, region.dropdown_anchor_y);
    }

    #[test]
    fn dropdown_anchor_survives_idle_rebuild() {
        // Regression: the CPU `on_frame` runs `update_interaction`
        // (which rebuilds regions) every frame, but gates `render`
        // behind a repaint check. On an idle frame the rebuild ran
        // without a following render, resetting `dropdown_anchor_y`
        // to 0 and stranding the next dropdown popup at the top of
        // the window. The rebuild must preserve the anchor.
        let mut editor = make_editor();

        // Simulate an idle frame: regions rebuilt, no render after.
        update_interaction(&mut editor);

        let (dx, dy) = dropdown_center(&editor);
        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);

        let dd = editor.interaction.dropdown.as_ref().unwrap();
        let region = &editor.interaction.knob_regions[dd.region_idx];
        assert_eq!(dd.popup_rect.1, region.dropdown_anchor_y);
        assert!(
            dd.popup_rect.1 > region.y,
            "popup_y {} fell back to the window top instead of anchoring below the button",
            dd.popup_rect.1
        );
    }

    #[test]
    fn dropdown_anchor_gap_stable_with_sections() {
        let editor_plain = make_editor();
        let editor_sections = make_editor_with_sections();

        let r_plain = editor_plain
            .interaction
            .knob_regions
            .iter()
            .find(|r| r.widget_type == WidgetType::Dropdown)
            .unwrap();
        let r_sections = editor_sections
            .interaction
            .knob_regions
            .iter()
            .find(|r| r.widget_type == WidgetType::Dropdown)
            .unwrap();

        // The gap from widget vertical center to anchor should be identical
        // regardless of section offsets shifting the absolute Y position.
        let gap_plain = r_plain.dropdown_anchor_y - (r_plain.y + r_plain.h / 2.0);
        let gap_sections = r_sections.dropdown_anchor_y - (r_sections.y + r_sections.h / 2.0);
        assert!(
            (gap_plain - gap_sections).abs() < 0.1,
            "gap_plain={gap_plain}, gap_sections={gap_sections}"
        );
    }

    // -- Mock Params with a large enum (20 options) for overflow/scroll tests --

    struct ManyOptionParams {
        values: [AtomicU64; 2],
    }

    impl ManyOptionParams {
        fn new() -> Self {
            Self {
                values: [
                    AtomicU64::new(0.0f64.to_bits()),
                    AtomicU64::new(0.0f64.to_bits()),
                ],
            }
        }
    }

    impl truce_params::__private::Sealed for ManyOptionParams {}
    impl Params for ManyOptionParams {
        fn param_infos(&self) -> Vec<ParamInfo> {
            vec![
                ParamInfo {
                    id: 0,
                    name: "Note",
                    short_name: "Note",
                    group: "",
                    range: ParamRange::Enum { count: 20 },
                    default_plain: 0.0,
                    flags: ParamFlags::AUTOMATABLE,
                    unit: ParamUnit::None,
                    kind: ParamValueKind::Enum,
                    midi_map: None,
                    midi_channel: None,
                },
                ParamInfo {
                    id: 1,
                    name: "Gain",
                    short_name: "Gain",
                    group: "",
                    range: ParamRange::Linear { min: 0.0, max: 1.0 },
                    default_plain: 0.5,
                    flags: ParamFlags::AUTOMATABLE,
                    unit: ParamUnit::None,
                    kind: ParamValueKind::Float,
                    midi_map: None,
                    midi_channel: None,
                },
            ]
        }

        fn count(&self) -> usize {
            2
        }

        fn get_normalized(&self, id: u32) -> Option<f64> {
            self.values
                .get(id as usize)
                .map(|v| f64::from_bits(v.load(Ordering::Relaxed)))
        }

        fn set_normalized(&self, id: u32, value: f64) {
            if let Some(v) = self.values.get(id as usize) {
                v.store(value.to_bits(), Ordering::Relaxed);
            }
        }

        fn get_plain(&self, id: u32) -> Option<f64> {
            let norm = self.get_normalized(id)?;
            let info = self.param_infos().iter().find(|i| i.id == id).copied()?;
            Some(info.range.denormalize(norm))
        }

        fn set_plain(&self, id: u32, value: f64) {
            if let Some(info) = self.param_infos().iter().find(|i| i.id == id).copied() {
                self.set_normalized(id, info.range.normalize(value));
            }
        }

        fn format_value(&self, _id: u32, value: f64) -> Option<String> {
            Some(format!("{value:.0}"))
        }

        fn parse_value(&self, _id: u32, _text: &str) -> Option<f64> {
            None
        }
        fn snap_smoothers(&self) {}
        fn set_sample_rate(&self, _: f64) {}

        fn collect_values(&self) -> (Vec<u32>, Vec<f64>) {
            let ids = vec![0, 1];
            let vals: Vec<f64> = ids
                .iter()
                .map(|&id| self.get_plain(id).unwrap_or(0.0))
                .collect();
            (ids, vals)
        }

        fn restore_values(&self, values: &[(u32, f64)]) {
            for &(id, val) in values {
                self.set_plain(id, val);
            }
        }
    }

    impl Default for ManyOptionParams {
        fn default() -> Self {
            Self::new()
        }
    }

    // -- Additional helpers --

    /// Build an editor with a dropdown in the last row (near the window bottom).
    fn make_editor_bottom_dropdown() -> BuiltinEditor<TestParams> {
        let params = Arc::new(TestParams::new());
        // 3 rows of 2, dropdown in the last row (row 2)
        let layout = GridLayout::build(vec![widgets(vec![
            GridWidget::knob(1u32, "K1"),
            GridWidget::knob(1u32, "K2"),
            GridWidget::knob(1u32, "K3"),
            GridWidget::knob(1u32, "K4"),
            GridWidget::dropdown(0u32, "Mode"),
            GridWidget::knob(1u32, "K5"),
        ])])
        .with_cols(2);
        let mut editor = BuiltinEditor::new_grid(params, layout);
        if let Layout::Grid(ref gl) = editor.layout {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type =
                        resolve_widget_type(gw.widget, gw.param_id, &*editor.params);
                }
            }
        }
        editor.render();
        editor
    }

    /// Build an editor with two dropdowns side by side.
    fn make_editor_two_dropdowns() -> BuiltinEditor<TestParams> {
        let params = Arc::new(TestParams::new());
        let layout = GridLayout::build(vec![widgets(vec![
            GridWidget::dropdown(0u32, "Mode A"),
            GridWidget::dropdown(0u32, "Mode B"),
        ])]);
        let mut editor = BuiltinEditor::new_grid(params, layout);
        if let Layout::Grid(ref gl) = editor.layout {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type =
                        resolve_widget_type(gw.widget, gw.param_id, &*editor.params);
                }
            }
        }
        editor.render();
        editor
    }

    /// Build an editor with a 20-option dropdown for scroll testing.
    fn make_editor_many_options() -> BuiltinEditor<ManyOptionParams> {
        let params = Arc::new(ManyOptionParams::new());
        let layout = GridLayout::build(vec![widgets(vec![
            GridWidget::dropdown(0u32, "Note"),
            GridWidget::knob(1u32, "Gain"),
        ])]);
        let mut editor = BuiltinEditor::new_grid(params, layout);
        if let Layout::Grid(ref gl) = editor.layout {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type =
                        resolve_widget_type(gw.widget, gw.param_id, &*editor.params);
                }
            }
        }
        editor.render();
        editor
    }

    fn dropdown_center_many(editor: &BuiltinEditor<ManyOptionParams>) -> (f32, f32) {
        let region = editor
            .interaction
            .knob_regions
            .iter()
            .find(|r| r.widget_type == WidgetType::Dropdown)
            .expect("no dropdown in layout");
        (region.x + region.w / 2.0, region.y + region.h / 2.0)
    }

    // -- Tests: dropdown overflow/clipping --

    #[test]
    fn dropdown_anchors_below_button_scrolls_when_tight() {
        let mut editor = make_editor_bottom_dropdown();
        let (dx, dy) = {
            let region = editor
                .interaction
                .knob_regions
                .iter()
                .find(|r| r.widget_type == WidgetType::Dropdown)
                .unwrap();
            (region.x + region.w / 2.0, region.y + region.h / 2.0)
        };

        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);
        assert!(editor.interaction.dropdown_is_open());

        let dd = editor.interaction.dropdown.as_ref().unwrap();
        let region = &editor.interaction.knob_regions[dd.region_idx];
        let (_, popup_y, _, popup_h) = dd.popup_rect;
        let window_h = editor.layout.height() as f32;

        // Popup anchors at the button's bottom - never shifts up
        // and never flips above. If the full option list doesn't
        // fit between the anchor and the window bottom, the popup
        // scrolls instead of relocating away from the tap target.
        assert_eq!(
            popup_y, region.dropdown_anchor_y,
            "popup must anchor at dropdown_anchor_y, got popup_y={popup_y}"
        );
        // Popup never extends past the window bottom.
        assert!(
            popup_y + popup_h <= window_h + 1.0,
            "popup bottom {} exceeds window height {window_h}",
            popup_y + popup_h
        );
    }

    #[test]
    fn dropdown_clamps_horizontal_near_right_edge() {
        let mut editor = make_editor_two_dropdowns();
        // The second dropdown is in column 1 (right side)
        let region = &editor.interaction.knob_regions[1];
        assert_eq!(region.widget_type, WidgetType::Dropdown);
        let dx = region.x + region.w / 2.0;
        let dy = region.y + region.h / 2.0;

        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);
        assert!(editor.interaction.dropdown_is_open());

        let dd = editor.interaction.dropdown.as_ref().unwrap();
        let (popup_x, _, popup_w, _) = dd.popup_rect;
        let window_w = editor.layout.width() as f32;

        assert!(
            popup_x + popup_w <= window_w + 1.0,
            "popup right edge {} exceeds window width {window_w}",
            popup_x + popup_w
        );
        assert!(popup_x >= 0.0, "popup_x={popup_x} is negative");
    }

    #[test]
    fn dropdown_scroll_long_list() {
        let mut editor = make_editor_many_options();
        let (dx, dy) = dropdown_center_many(&editor);

        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);
        assert!(editor.interaction.dropdown_is_open());

        let dd = editor.interaction.dropdown.as_ref().unwrap();
        // 20-option enum → step_count = 19 → 19 options
        assert!(
            dd.options.len() > dd.visible_count,
            "expected scroll: {} options, {} visible",
            dd.options.len(),
            dd.visible_count
        );
        assert_eq!(dd.scroll_offset, 0);
    }

    #[test]
    fn dropdown_scroll_clamps_to_bounds() {
        let mut editor = make_editor_many_options();
        let (dx, dy) = dropdown_center_many(&editor);

        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);

        // Scroll up past the top - should stay at 0
        editor.interaction.dropdown_scroll(-10);
        assert_eq!(
            editor.interaction.dropdown.as_ref().unwrap().scroll_offset,
            0
        );

        // Scroll down past the bottom - should clamp
        editor.interaction.dropdown_scroll(1000);
        let dd = editor.interaction.dropdown.as_ref().unwrap();
        let max_offset = dd.options.len().saturating_sub(dd.visible_count);
        assert_eq!(dd.scroll_offset, max_offset);
    }

    #[test]
    fn dropdown_selected_item_visible_on_open() {
        let mut editor = make_editor_many_options();
        // Set the value to option 15 out of 19 (normalized = 15/18)
        editor.params.set_normalized(0, 15.0 / 18.0);

        let (dx, dy) = dropdown_center_many(&editor);
        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);

        let dd = editor.interaction.dropdown.as_ref().unwrap();
        let selected = dd.selected;
        // The selected item should be within the visible window
        assert!(
            selected >= dd.scroll_offset && selected < dd.scroll_offset + dd.visible_count,
            "selected={selected} not in visible range {}..{}",
            dd.scroll_offset,
            dd.scroll_offset + dd.visible_count
        );
    }

    #[test]
    fn dropdown_scroll_then_select_correct_index() {
        let mut editor = make_editor_many_options();
        let (dx, dy) = dropdown_center_many(&editor);

        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);

        // Scroll down by 3
        editor.interaction.dropdown_scroll(3);
        assert_eq!(
            editor.interaction.dropdown.as_ref().unwrap().scroll_offset,
            3
        );

        // Click the second visible item (local index 1 → absolute index 4)
        let dd = editor.interaction.dropdown.as_ref().unwrap();
        let (px, py, _, _) = dd.popup_rect;
        let item_h = 18.0f32;
        let padding = 4.0f32;
        let click_y = py + padding + item_h + item_h / 2.0; // middle of second visible item

        editor.on_mouse_down(px + 10.0, click_y);
        editor.on_mouse_up(px + 10.0, click_y);

        assert!(!editor.interaction.dropdown_is_open());
        // Absolute index = scroll_offset(3) + local(1) = 4
        // 20 options → norm = 4/19
        let norm = editor.params.get_normalized(0).unwrap();
        let expected = 4.0 / 19.0;
        assert!(
            (norm - expected).abs() < 0.01,
            "expected {expected:.4}, got {norm:.4}"
        );
    }

    #[test]
    fn dropdown_click_different_dropdown_closes_first() {
        let mut editor = make_editor_two_dropdowns();
        let r0 = &editor.interaction.knob_regions[0];
        let r1 = &editor.interaction.knob_regions[1];
        let (ax, ay) = (r0.x + r0.w / 2.0, r0.y + r0.h / 2.0);
        let (bx, by) = (r1.x + r1.w / 2.0, r1.y + r1.h / 2.0);

        // Open dropdown A
        editor.on_mouse_down(ax, ay);
        editor.on_mouse_up(ax, ay);
        assert!(editor.interaction.dropdown_is_open());
        assert_eq!(editor.interaction.dropdown.as_ref().unwrap().region_idx, 0);

        // Click dropdown B - should close A and open B
        editor.on_mouse_down(bx, by);
        editor.on_mouse_up(bx, by);
        assert!(editor.interaction.dropdown_is_open());
        assert_eq!(editor.interaction.dropdown.as_ref().unwrap().region_idx, 1);
    }

    #[test]
    fn dropdown_hover_tracks_correct_option() {
        let mut editor = make_editor();
        let (dx, dy) = dropdown_center(&editor);

        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);

        let dd = editor.interaction.dropdown.as_ref().unwrap();
        let (px, py, pw, _) = dd.popup_rect;
        let item_h = 18.0f32;
        let padding = 4.0f32;
        let last_visible = dd.visible_count - 1;

        // Hover over the last visible item
        let hover_y = py + padding + last_visible as f32 * item_h + item_h / 2.0;
        editor.on_mouse_moved(px + pw / 2.0, hover_y);

        let dd = editor.interaction.dropdown.as_ref().unwrap();
        assert_eq!(
            dd.hover_option,
            Some(last_visible),
            "expected hover on last visible option"
        );

        // Move outside the popup
        editor.on_mouse_moved(0.0, 0.0);
        let dd = editor.interaction.dropdown.as_ref().unwrap();
        assert_eq!(dd.hover_option, None, "hover should clear outside popup");
    }

    #[test]
    fn dropdown_popup_within_window_bounds() {
        // Verify popup never exceeds window in any direction
        let mut editor = make_editor();
        let (dx, dy) = dropdown_center(&editor);

        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);

        let dd = editor.interaction.dropdown.as_ref().unwrap();
        let (px, py, pw, ph) = dd.popup_rect;
        let window_w = editor.layout.width() as f32;
        let window_h = editor.layout.height() as f32;

        assert!(px >= 0.0, "popup left edge {px} < 0");
        assert!(py >= 0.0, "popup top edge {py} < 0");
        assert!(
            px + pw <= window_w + 1.0,
            "popup right {} > window {window_w}",
            px + pw
        );
        assert!(
            py + ph <= window_h + 1.0,
            "popup bottom {} > window {window_h}",
            py + ph
        );
    }
}
