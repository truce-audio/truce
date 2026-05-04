//! Built-in editor using the CPU render backend.
//!
//! Renders parameter widgets via `RenderBackend`. Uses tiny-skia for
//! software rasterization and baseview + wgpu for window management
//! and blitting. For GPU-accelerated rendering see the `truce-gpu`
//! crate which provides `GpuEditor` wrapping this editor.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use truce_core::cast::param_f32;
use truce_core::editor::{Editor, PluginContext, RawWindowHandle};
use truce_params::Params;

use crate::backend_cpu::CpuBackend;
use crate::interaction::{self, InputEvent, InteractionState, ParamEdit};
use crate::layout::{GridLayout, Layout, PluginLayout};
use crate::platform::EditorScale;
use crate::render::RenderBackend;
use crate::snapshot::ParamSnapshot;
use crate::theme::Theme;
use crate::widgets::{self, WidgetType};

/// Owned `'static` closures that back a `ParamSnapshot` for the current
/// frame. Each closure captures an `Arc` of the params / context, so the
/// struct can live across a separate `&mut self.interaction` borrow.
struct EditorSnapshotClosures {
    get_param: Box<dyn Fn(u32) -> f32>,
    get_param_plain: Box<dyn Fn(u32) -> f32>,
    format_param: Box<dyn Fn(u32) -> String>,
    get_meter: Box<dyn Fn(u32) -> f32>,
    get_options: Box<dyn Fn(u32) -> Vec<String>>,
    default_normalized: Box<dyn Fn(u32) -> f32>,
    next_discrete_normalized: Box<dyn Fn(u32) -> f32>,
    param_name: Box<dyn Fn(u32) -> String>,
    widget_type: Box<dyn Fn(u32) -> WidgetType>,
}

impl EditorSnapshotClosures {
    fn as_snapshot(&self) -> ParamSnapshot<'_> {
        ParamSnapshot {
            get_param: &*self.get_param,
            get_param_plain: &*self.get_param_plain,
            format_param: &*self.format_param,
            get_meter: &*self.get_meter,
            get_options: &*self.get_options,
            default_normalized: &*self.default_normalized,
            next_discrete_normalized: &*self.next_discrete_normalized,
            param_name: &*self.param_name,
            widget_type: &*self.widget_type,
        }
    }
}

/// Built-in editor that renders parameter widgets to a pixel buffer.
///
/// Uses the CPU backend (tiny-skia) for software rasterization. When
/// `open()` is called, creates a baseview window and blits pixels via wgpu.
pub struct BuiltinEditor<P: Params> {
    params: Arc<P>,
    layout: Layout,
    theme: Theme,
    backend: Option<CpuBackend>,
    interaction: InteractionState,
    context: Option<PluginContext>,
    window: Option<baseview::WindowHandle>,
    /// Weak-ish handle to the blit backend the window-handler
    /// materializes. The editor keeps the canonical `Arc` and the
    /// handler gets a clone. On close we take the `Option` out of
    /// the inner mutex — dropping the wgpu Surface synchronously —
    /// before asking baseview to tear the `NSView` down.
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
    /// host-driven param changes (automation, preset recall) — if any
    /// live value drifts from the last-painted one, we force a
    /// repaint even if the UI never received a direct edit.
    last_painted_values: Vec<f32>,
    /// Live content-scale factor, shared with the baseview handler via
    /// [`crate::platform::EditorScale`]. `set_scale_factor` (host)
    /// writes the cell; the handler holds a clone, compares against
    /// `last_applied_scale` each frame, and rebuilds the CPU pixmap +
    /// reconfigures the wgpu surface when the value diverges. Single
    /// source of truth shared with egui / iced / slint / gpu backends.
    scale: EditorScale,
}

// SAFETY: `baseview::WindowHandle` holds a raw native window pointer
// (HWND / NSView / X11 Window) and is not auto-`Send`. Hosts call
// `Editor::open` / `idle` / `close` from a single dedicated GUI thread
// — never concurrently and never from the audio thread — so the
// handle is only ever touched on the thread that created it. The
// `Editor` trait requires `Send` so the editor can live behind a
// trait object; this impl asserts that the type doesn't escape its
// thread in practice. All other fields (`Arc<P>`, `Layout`, `Theme`,
// `Option<CpuBackend>`, etc.) are themselves `Send`.
unsafe impl<P: Params> Send for BuiltinEditor<P> {}

impl<P: Params + 'static> BuiltinEditor<P> {
    /// Request a repaint on the next idle tick. Call this if plugin
    /// code mutates display state outside the normal param or
    /// `state_changed` pathways (uncommon). User interaction and
    /// host automation already flag themselves dirty automatically.
    pub fn request_repaint(&self) {
        self.needs_repaint.store(true, Ordering::Release);
    }

    fn take_needs_repaint(&self) -> bool {
        self.needs_repaint.swap(false, Ordering::AcqRel)
    }

    /// Compare the values just read by `update_interaction` (live from
    /// the host / params Arc) against those captured at the last
    /// render. A mismatch means an automation lane wrote a new value,
    /// a preset was recalled, or some other off-UI state change
    /// happened — force a repaint so the widget tracks it.
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
    /// automation detection. Called after each render.
    fn stash_painted_values(&mut self) {
        let regions = &self.interaction.knob_regions;
        // Resize-then-overwrite reuses the existing allocation
        // unchanged when the region count is steady (the common
        // case — knob layouts only change on
        // `interaction.build_regions`). The previous
        // clear-then-extend form pumped through the iterator path
        // every frame even when the length didn't change.
        self.last_painted_values.resize(regions.len(), 0.0);
        for (slot, region) in self.last_painted_values.iter_mut().zip(regions.iter()) {
            *slot = region.normalized_value;
        }
    }

    pub fn new(params: Arc<P>, layout: PluginLayout) -> Self {
        Self {
            params,
            layout: Layout::Rows(layout),
            theme: Theme::dark(),
            backend: None,
            interaction: InteractionState::new(),
            context: None,
            window: None,
            blit_backend: None,
            needs_repaint: Arc::new(AtomicBool::new(false)),
            last_painted_values: Vec::new(),
            scale: EditorScale::new(crate::backing_scale()),
        }
    }

    pub fn new_with_layout(params: Arc<P>, layout: Layout) -> Self {
        Self {
            params,
            layout,
            theme: Theme::dark(),
            backend: None,
            interaction: InteractionState::new(),
            context: None,
            window: None,
            blit_backend: None,
            needs_repaint: Arc::new(AtomicBool::new(false)),
            last_painted_values: Vec::new(),
            scale: EditorScale::new(crate::backing_scale()),
        }
    }

    pub fn new_grid(params: Arc<P>, layout: GridLayout) -> Self {
        Self {
            params,
            layout: Layout::Grid(layout),
            theme: Theme::dark(),
            backend: None,
            interaction: InteractionState::new(),
            context: None,
            window: None,
            blit_backend: None,
            needs_repaint: Arc::new(AtomicBool::new(false)),
            last_painted_values: Vec::new(),
            scale: EditorScale::new(crate::backing_scale()),
        }
    }

    #[must_use]
    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Render the full UI to the internal CPU pixel buffer.
    ///
    /// # Panics
    ///
    /// Panics if the lazy `CpuBackend::new` allocation fails (out of
    /// memory or zero dimensions). The backend is allocated on first
    /// render — subsequent calls reuse it.
    pub fn render(&mut self) {
        let (w, h) = (self.layout.width(), self.layout.height());
        let scale = self.scale.get_f32();
        let owned = self.build_snapshot_closures();
        let snapshot = owned.as_snapshot();
        let backend = self
            .backend
            .get_or_insert_with(|| CpuBackend::new(w, h, scale).expect("Failed to create backend"));
        Self::render_widgets(
            &self.layout,
            &self.theme,
            &mut self.interaction,
            &snapshot,
            backend,
        );
    }

    /// Render all widgets to a `RenderBackend`. Takes split borrows of
    /// the relevant editor fields rather than `&mut self`, so callers
    /// can hold `&mut self.backend` (or pass an external backend) at
    /// the same time.
    fn render_widgets(
        layout: &Layout,
        theme: &Theme,
        interaction: &mut InteractionState,
        snapshot: &ParamSnapshot<'_>,
        backend: &mut dyn RenderBackend,
    ) {
        // `widgets::draw` does not clear; do it here so the built-in
        // editor's background matches the theme.
        backend.clear(theme.background);
        widgets::draw(backend, layout, theme, snapshot, interaction);
    }

    /// Build owned boxed closures from `self.context` / `self.params` that
    /// back a `ParamSnapshot`. Each closure clones the `Arc<P>` or the
    /// `PluginContext`, so `EditorSnapshotClosures` is `'static` and safe
    /// to hold across a borrow of `&mut self.interaction`.
    fn build_snapshot_closures(&self) -> EditorSnapshotClosures {
        let ctx = self.context.clone();
        let p = Arc::clone(&self.params);
        let p_get = Arc::clone(&p);
        let p_get_plain = Arc::clone(&p);
        let p_fmt = Arc::clone(&p);
        let p_opts = Arc::clone(&p);
        let p_default = Arc::clone(&p);
        let p_next = Arc::clone(&p);
        let p_name = Arc::clone(&p);
        let p_wtype = Arc::clone(&p);

        let get_param: Box<dyn Fn(u32) -> f32> = match &ctx {
            Some(c) => {
                let c = c.clone();
                Box::new(move |id| param_f32(c.get_param(id)))
            }
            None => Box::new(move |id| param_f32(p_get.get_normalized(id).unwrap_or(0.0))),
        };
        let get_param_plain: Box<dyn Fn(u32) -> f32> = match &ctx {
            Some(c) => {
                let c = c.clone();
                Box::new(move |id| param_f32(c.get_param_plain(id)))
            }
            None => Box::new(move |id| param_f32(p_get_plain.get_plain(id).unwrap_or(0.0))),
        };
        let format_param: Box<dyn Fn(u32) -> String> = match &ctx {
            Some(c) => {
                let c = c.clone();
                Box::new(move |id| c.format_param(id))
            }
            None => Box::new(move |id| {
                let v = p_fmt.get_plain(id).unwrap_or(0.0);
                p_fmt
                    .format_value(id, v)
                    .unwrap_or_else(|| format!("{v:.1}"))
            }),
        };
        let get_meter: Box<dyn Fn(u32) -> f32> = match &ctx {
            Some(c) => {
                let c = c.clone();
                Box::new(move |id| c.get_meter(id))
            }
            None => Box::new(move |_| 0.0),
        };
        let get_options: Box<dyn Fn(u32) -> Vec<String>> = Box::new(move |id| {
            let Some(info) = p_opts.param_infos().into_iter().find(|i| i.id == id) else {
                return Vec::new();
            };
            let count = info.range.step_count().map_or(1, |n| n.get() as usize) + 1;
            (0..count)
                .map(|i| {
                    let norm = truce_core::cast::discrete_norm(i, count);
                    let plain = info.range.denormalize(norm);
                    p_opts
                        .format_value(id, plain)
                        .unwrap_or_else(|| format!("{plain:.0}"))
                })
                .collect()
        });
        let default_normalized: Box<dyn Fn(u32) -> f32> =
            Box::new(
                move |id| match p_default.param_infos().iter().find(|i| i.id == id) {
                    Some(info) => param_f32(info.range.normalize(info.default_plain)),
                    None => 0.0,
                },
            );
        let next_discrete_normalized: Box<dyn Fn(u32) -> f32> = Box::new(move |id| {
            let Some(info) = p_next.param_infos().into_iter().find(|i| i.id == id) else {
                return 0.0;
            };
            let plain = p_next.get_plain(id).unwrap_or(0.0);
            let max = info.range.max();
            let next = if plain >= max { 0.0 } else { plain + 1.0 };
            param_f32(info.range.normalize(next))
        });
        let param_name: Box<dyn Fn(u32) -> String> = Box::new(move |id| {
            p_name
                .param_infos()
                .into_iter()
                .find(|i| i.id == id)
                .map(|i| i.name.to_string())
                .unwrap_or_default()
        });
        let widget_type: Box<dyn Fn(u32) -> WidgetType> = Box::new(move |id| {
            let info = p_wtype.param_infos().into_iter().find(|i| i.id == id);
            match info.as_ref().map(|i| &i.range) {
                Some(truce_params::ParamRange::Discrete { min: 0, max: 1 }) => WidgetType::Toggle,
                Some(truce_params::ParamRange::Enum { .. }) => WidgetType::Selector,
                _ => WidgetType::Knob,
            }
        });

        EditorSnapshotClosures {
            get_param,
            get_param_plain,
            format_param,
            get_meter,
            get_options,
            default_normalized,
            next_discrete_normalized,
            param_name,
            widget_type,
        }
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

    /// Render all widgets to an external `RenderBackend`.
    ///
    /// Used by `truce-gpu` to draw through the GPU backend instead of
    /// the internal CPU backend.
    pub fn render_to(&mut self, backend: &mut dyn RenderBackend) {
        update_interaction(self);
        let owned = self.build_snapshot_closures();
        let snapshot = owned.as_snapshot();
        Self::render_widgets(
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
            x,
            y,
            button: crate::interaction::MouseButton::Left,
        }]);
    }

    fn on_mouse_up(&mut self, x: f32, y: f32) {
        self.dispatch_events(&[InputEvent::MouseUp {
            x,
            y,
            button: crate::interaction::MouseButton::Left,
        }]);
    }

    fn on_mouse_moved(&mut self, x: f32, y: f32) {
        self.dispatch_events(&[InputEvent::MouseMove { x, y }]);
    }
}

// ---------------------------------------------------------------------------
// C callbacks — thin wrappers that cast the context pointer back to &mut Self
// ---------------------------------------------------------------------------

/// Update interaction regions and live param values.
///
/// Takes `&mut BuiltinEditor<P>` so the borrow checker enforces
/// non-aliasing — the function only touches Rust references and is
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
            region.normalized_value = param_f32(ctx.get_param(region.param_id));
        } else {
            region.normalized_value =
                param_f32(editor.params.get_normalized(region.param_id).unwrap_or(0.0));
        }
    }
}

// ---------------------------------------------------------------------------
// Baseview WindowHandler — drives the CPU render loop
// ---------------------------------------------------------------------------
//
// On macOS + AAX: blits via CoreGraphics (CGImage → CALayer) to avoid Metal
// autorelease crashes with multiple editor windows.
// Otherwise: blits via wgpu fullscreen triangle.

fn create_wgpu_backend(window: &mut baseview::Window, phys_w: u32, phys_h: u32) -> BlitBackend {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });

    let surface = unsafe { crate::platform::create_wgpu_surface(&instance, window) }
        .expect("failed to create wgpu surface");

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))
    .expect("no suitable GPU adapter");

    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("truce-gui"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            memory_hints: wgpu::MemoryHints::Performance,
        },
        None,
    ))
    .expect("failed to create wgpu device");

    let caps = surface.get_capabilities(&adapter);
    let format = caps
        .formats
        .iter()
        .find(|f| f.is_srgb())
        .copied()
        .unwrap_or(caps.formats[0]);

    let surface_config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: phys_w,
        height: phys_h,
        present_mode: wgpu::PresentMode::AutoVsync,
        desired_maximum_frame_latency: 2,
        alpha_mode: wgpu::CompositeAlphaMode::Auto,
        view_formats: vec![],
    };
    surface.configure(&device, &surface_config);

    // Blit texture matches the CPU pixmap, which is now sized at
    // physical pixels (see CpuBackend's scale handling). With texture
    // and surface at the same physical size, the full-screen-triangle
    // blit samples 1:1 — no stretch, no Retina blur.
    let blit = crate::blit::BlitPipeline::new(&device, format, phys_w, phys_h);

    BlitBackend {
        blit,
        surface_config,
        surface,
        queue,
        device,
    }
}

// Field-declaration order doubles as the implicit drop order Rust uses
// when this struct is dropped through the `Option<BlitBackend>` cell
// directly (e.g. when the host drops the editor without calling
// `close`). Children before parent: per-pipeline GPU resources, then
// the surface (releases swap chain / CAMetalLayer), then queue, then
// device. `BuiltinEditor::close` does the same thing explicitly via
// destructure — this declaration order keeps the implicit path safe
// too.
struct BlitBackend {
    blit: crate::blit::BlitPipeline,
    surface_config: wgpu::SurfaceConfiguration,
    surface: wgpu::Surface<'static>,
    queue: wgpu::Queue,
    device: wgpu::Device,
}

impl BlitBackend {
    /// Reconfigure the wgpu surface and blit texture for a new physical
    /// size. Used when `Editor::set_scale_factor` reports a host-driven
    /// DPI change — the logical editor size doesn't change, but the
    /// physical pixmap and surface need to grow / shrink to match.
    fn resize(&mut self, phys_w: u32, phys_h: u32) {
        self.surface_config.width = phys_w.max(1);
        self.surface_config.height = phys_h.max(1);
        self.surface.configure(&self.device, &self.surface_config);
        self.blit.resize(&self.device, phys_w, phys_h);
    }
}

/// Shared ownership of the blit backend between `BuiltinEditor` and the
/// `BuiltinWindowHandler` baseview hands us. Sharing lets the editor
/// drop the wgpu surface *before* it asks baseview to close the `NSView`
/// — important on AAX where interleaving Metal teardown with baseview's
/// close sequence inside Pro Tools' outer autorelease pool has been
/// seen to leave stale refs in DFW container views.
type SharedBackend = Arc<std::sync::Mutex<Option<BlitBackend>>>;

struct BuiltinWindowHandler<P: Params> {
    /// Raw pointer to the `BuiltinEditor` owned by the host. Valid only
    /// while `backend.lock()` returns `Some(_)`. `BuiltinEditor::close`
    /// takes the inner `Option<BlitBackend>` (atomically through this
    /// mutex) before returning, and the host can only drop the editor
    /// after `close()` returns — so any frame that holds the lock and
    /// finds the inner option `Some` is guaranteed the editor is still
    /// alive. Concretely, this lock acquire is the synchronization
    /// point that prevents the use-after-free that the audit flagged
    /// (an in-flight `on_frame` deref'ing a freed pointer if the host
    /// dropped the editor while baseview's render thread still had a
    /// callback queued). Only accessed from the GUI thread.
    editor: *mut BuiltinEditor<P>,
    backend: SharedBackend,
    /// Canonical baseview → `InputEvent` translator. Handles cursor
    /// tracking, double-click synthesis, and line→pixel scroll
    /// conversion once for everyone.
    translator: crate::interaction::BaseviewTranslator,
    /// Last scale we built the CPU pixmap + wgpu surface against.
    /// `on_frame` reads `editor.scale.get()` (via the raw ptr deref
    /// it already does) and compares; on divergence it rebuilds the
    /// pixmap and reconfigures the surface. Unlike egui / iced /
    /// slint we don't need a separate `EditorScale` clone on the
    /// handler — the editor is reachable through the same ptr that
    /// guards the lifecycle, so reading `editor.scale` is the
    /// canonical access path.
    last_applied_scale: f32,
}

// SAFETY: The raw pointer is only accessed from the GUI thread.
// baseview requires Send for WindowHandler.
unsafe impl<P: Params> Send for BuiltinWindowHandler<P> {}

impl<P: Params + 'static> baseview::WindowHandler for BuiltinWindowHandler<P> {
    fn on_frame(&mut self, _window: &mut baseview::Window) {
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
            // Nothing to do — baseview will tear us down next.
            return;
        }

        let editor = unsafe { &mut *self.editor };

        // Pick up host-driven scale changes (CLAP `set_scale`, VST3
        // `IPlugViewContentScaleSupport`) that landed in the shared
        // cell since the last frame. The OS-driven `Resized` path
        // intentionally stays a no-op (resize is disallowed per
        // `Editor::can_resize`'s `false` default), so this branch is
        // the only way scale changes propagate.
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
        if !editor.take_needs_repaint() {
            return;
        }
        editor.render();
        editor.stash_painted_values();

        if let Some(pixels) = editor.pixel_data() {
            let backend = guard
                .as_mut()
                .expect("guard was checked Some above and the lock is still held");
            let BlitBackend {
                device,
                queue,
                surface,
                blit,
                ..
            } = backend;
            blit.update(queue, pixels);
            let Ok(frame) = surface.get_current_texture() else {
                return;
            };
            let view = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder =
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            blit.render(&mut encoder, &view);
            queue.submit(std::iter::once(encoder.finish()));
            frame.present();
        }
    }

    fn on_event(
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
            // so we do it here. See truce-egui editor.rs.
            #[cfg(target_os = "windows")]
            {
                if !window.has_focus() {
                    window.focus();
                }
            }
        }

        // Lock-then-check-then-deref pattern, same as `on_frame` —
        // the backend cell is the synchronization point with
        // `BuiltinEditor::close`. If the cell is `None`, the editor
        // pointer is no longer guaranteed valid and we must not deref.
        let Ok(guard) = self.backend.lock() else {
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
                // Resize is intentionally disallowed: `Editor::can_resize`
                // and `Editor::set_size` use the trait defaults
                // (`false` / `false`), so hosts shouldn't drive a resize
                // through the truce protocol. We still pass the OS-
                // reported scale through `note_linux_scale_factor` so
                // newly opened editors on the same process see the
                // correct DPI from the cache, but we deliberately do
                // not resize the CPU pixmap or wgpu blit surface — a
                // user who drags the host window across a DPI boundary
                // accepts the stretched/cropped output. Matches the
                // `truce-gpu` `GpuEditor` posture so the two paths
                // behave identically.
                crate::platform::note_linux_scale_factor(info.scale());
                baseview::EventStatus::Ignored
            }
            _ => baseview::EventStatus::Ignored,
        }
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
        Some(crate::layout::WidgetKind::Selector) => widgets::WidgetType::Selector,
        Some(crate::layout::WidgetKind::Dropdown) => widgets::WidgetType::Dropdown,
        Some(crate::layout::WidgetKind::Meter) => widgets::WidgetType::Meter,
        Some(crate::layout::WidgetKind::XYPad) => widgets::WidgetType::XYPad,
        None => {
            let param_info = params.param_infos().into_iter().find(|i| i.id == param_id);
            match param_info.as_ref().map(|i| &i.range) {
                Some(truce_params::ParamRange::Discrete { min: 0, max: 1 }) => {
                    widgets::WidgetType::Toggle
                }
                Some(truce_params::ParamRange::Enum { .. }) => widgets::WidgetType::Selector,
                _ => widgets::WidgetType::Knob,
            }
        }
    }
}

impl<P: Params + 'static> Editor for BuiltinEditor<P> {
    fn size(&self) -> (u32, u32) {
        (self.layout.width(), self.layout.height())
    }

    fn state_changed(&mut self) {
        // Preset recall / undo / session load: params moved without
        // going through the UI, so force the next idle tick to repaint.
        self.request_repaint();
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        let (w, h) = self.size();
        // Refresh the shared scale from the parent window — on macOS
        // this is the live `[NSWindow backingScaleFactor]`, on
        // Windows the per-monitor DPI from the parent HWND. Any
        // `set_scale_factor` the host issues after open will overwrite
        // through the same shared cell.
        self.scale
            .set(crate::platform::query_backing_scale(&parent));
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
            scale: baseview::WindowScalePolicy::SystemScaleFactor,
        };

        let parent_wrapper = crate::platform::ParentWindow(parent);
        let editor_addr = std::ptr::from_mut::<BuiltinEditor<P>>(self) as usize;

        // Shared backend cell: the editor keeps one Arc and baseview's
        // window handler gets the other. At close time the editor
        // takes the inner Option and drops it *before* asking baseview
        // to tear down the NSView.
        let shared_backend: SharedBackend = Arc::new(std::sync::Mutex::new(None));
        self.blit_backend = Some(shared_backend.clone());
        let shared_for_handler = shared_backend;

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut baseview::Window| {
                let mut backend = create_wgpu_backend(window, phys_w, phys_h);

                // Render + present an initial frame synchronously, before
                // baseview shows the window. Without this, the window briefly
                // displays whatever garbage is in the surface buffer until the
                // first `on_frame` tick — especially noticeable on VST2
                // (Windows), where `effEditOpen` creates and shows the window
                // in one call.
                let editor = unsafe { &mut *(editor_addr as *mut BuiltinEditor<P>) };
                editor.render();
                if let Some(pixels) = editor.pixel_data() {
                    let BlitBackend {
                        device,
                        queue,
                        surface,
                        blit,
                        ..
                    } = &mut backend;
                    blit.update(queue, pixels);
                    if let Ok(frame) = surface.get_current_texture() {
                        let view = frame
                            .texture
                            .create_view(&wgpu::TextureViewDescriptor::default());
                        let mut encoder =
                            device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                                label: None,
                            });
                        blit.render(&mut encoder, &view);
                        queue.submit(std::iter::once(encoder.finish()));
                        frame.present();
                    }
                }

                // Publish the backend into the shared cell. If the
                // editor has already been asked to close (very
                // unlikely race — only if close fires before baseview
                // calls our build closure), the None-check on the
                // mutex side will simply replace Some(None) → Some
                // and everything drops at the usual time.
                if let Ok(mut guard) = shared_for_handler.lock() {
                    *guard = Some(backend);
                }

                BuiltinWindowHandler {
                    editor: editor_addr as *mut BuiltinEditor<P>,
                    backend: shared_for_handler.clone(),
                    translator: crate::interaction::BaseviewTranslator::new(),
                    last_applied_scale: scale_f32,
                }
            },
        );

        self.window = Some(window);
    }

    fn set_scale_factor(&mut self, factor: f64) {
        // Write to the shared cell; the baseview handler picks up the
        // change on its next frame and rebuilds the CPU pixmap +
        // reconfigures the wgpu surface. Replaces the default no-op
        // (host scale was previously dropped on the floor for the CPU
        // path, the only backend not yet on `EditorScale`).
        self.scale.set(factor);
    }

    fn close(&mut self) {
        // On macOS, wrap the teardown in an autoreleasepool so
        // anything baseview / wgpu / AppKit autoreleases during the
        // view's cleanup drains here rather than escaping into the
        // host's outer pool. See `../baseview/docs/pro-tools-aax-fix.md`
        // for why this matters on AAX.
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
        // makes the drop order explicit rather than relying on
        // `BlitPipeline`'s field-declaration order, since the audit
        // flagged "happens to work" reliance on Rust's drop semantics
        // as a fragility hazard. Order: per-pipeline GPU resources
        // first (textures, bind groups, sampler), then the surface
        // (releases the swap chain / CAMetalLayer), then queue, then
        // device last — children before parent.
        if let Some(shared) = self.blit_backend.take()
            && let Ok(mut guard) = shared.lock()
            && let Some(backend) = guard.take()
        {
            let BlitBackend {
                blit,
                surface,
                surface_config,
                queue,
                device,
            } = backend;
            drop(surface_config);
            drop(blit);
            drop(surface);
            drop(queue);
            drop(device);
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
    use truce_params::{ParamFlags, ParamInfo, ParamRange, ParamUnit, Params};

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
            let info = self.param_infos().into_iter().find(|i| i.id == id)?;
            Some(info.range.denormalize(norm))
        }

        fn set_plain(&self, id: u32, value: f64) {
            if let Some(info) = self.param_infos().into_iter().find(|i| i.id == id) {
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

        // Click same button again — should close, not reopen
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

        editor.on_mouse_down(px + 10.0, option_y);

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

        // popup_rect.1 (popup_y) must equal the stored anchor
        assert_eq!(dd.popup_rect.1, region.dropdown_anchor_y);
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
            let info = self.param_infos().into_iter().find(|i| i.id == id)?;
            Some(info.range.denormalize(norm))
        }

        fn set_plain(&self, id: u32, value: f64) {
            if let Some(info) = self.param_infos().into_iter().find(|i| i.id == id) {
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
    fn dropdown_flips_upward_when_near_bottom() {
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

        // Popup should not extend past the window bottom
        assert!(
            popup_y + popup_h <= window_h + 1.0, // +1 for float rounding
            "popup bottom {} exceeds window height {window_h}",
            popup_y + popup_h
        );

        // If it flipped, popup_y should be above the button
        let anchor_below = region.dropdown_anchor_y;
        let anchor_above = anchor_below - 20.0;
        let item_h = 18.0f32;
        let padding = 4.0f32;
        let full_h = dd.options.len() as f32 * item_h + padding * 2.0;
        if anchor_below + full_h > window_h {
            // Should have flipped upward: popup top is above the button top
            assert!(
                popup_y < anchor_above,
                "expected upward flip: popup_y={popup_y}, anchor_above={anchor_above}"
            );
        }
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

        // Scroll up past the top — should stay at 0
        editor.interaction.dropdown_scroll(-10);
        assert_eq!(
            editor.interaction.dropdown.as_ref().unwrap().scroll_offset,
            0
        );

        // Scroll down past the bottom — should clamp
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

        // Click dropdown B — should close A and open B
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
