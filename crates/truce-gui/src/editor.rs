//! Built-in editor using the CPU render backend.
//!
//! Renders parameter widgets via `RenderBackend`. Uses tiny-skia for
//! software rasterization and baseview for window management.
//! On macOS, pixel blitting uses CoreGraphics (CGImage → CALayer) when
//! requested via [`request_cg_blit`], avoiding Metal/wgpu entirely.
//! Otherwise uses wgpu for blitting. For GPU rendering, see the
//! `truce-gpu` crate which provides `GpuEditor` wrapping this editor.

use std::sync::Arc;

use truce_core::editor::{Editor, EditorContext, RawWindowHandle};
use truce_params::Params;

// ---------------------------------------------------------------------------
// CoreGraphics blit opt-in (used by AAX to avoid Metal autorelease crashes)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "macos")]
static USE_CG_BLIT: AtomicBool = AtomicBool::new(false);

/// Request that the built-in GUI use CoreGraphics blitting instead of wgpu
/// on macOS. Call this before `editor.open()`. This avoids Metal/wgpu
/// resources that cause autorelease pool crashes when multiple editors
/// coexist in the same process (e.g. AAX in Pro Tools).
#[cfg(target_os = "macos")]
pub fn request_cg_blit(enable: bool) {
    USE_CG_BLIT.store(enable, Ordering::Relaxed);
}

/// Returns true if the CgBlit path was requested (AAX on macOS).
#[cfg(target_os = "macos")]
pub fn should_use_cg_blit() -> bool {
    USE_CG_BLIT.load(Ordering::Relaxed)
}


/// No-op on non-macOS platforms.
#[cfg(not(target_os = "macos"))]
pub fn request_cg_blit(_enable: bool) {}

/// Always false on non-macOS.
#[cfg(not(target_os = "macos"))]
pub fn should_use_cg_blit() -> bool {
    false
}

use crate::backend_cpu::CpuBackend;
use crate::interaction::{self, InputEvent, InteractionState, MouseButton, ParamEdit};
use crate::layout::{GridLayout, Layout, PluginLayout};
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
    context: Option<EditorContext>,
    window: Option<baseview::WindowHandle>,
    #[cfg(target_os = "macos")]
    native: Option<crate::native_view::NativeView>,
    /// Weak-ish handle to the blit backend the window-handler
    /// materializes. The editor keeps the canonical `Arc` and the
    /// handler gets a clone. On close we take the `Option` out of
    /// the inner mutex — dropping the wgpu Surface synchronously —
    /// before asking baseview to tear the NSView down.
    blit_backend: Option<SharedBackend>,
    /// Set whenever something visible changes (param edited via the
    /// UI, host-driven state reload, explicit `request_repaint` by
    /// plugin code). `on_frame` clears it and only does the
    /// rasterize + blit pass when it was true.
    ///
    /// Shared so `EditorContext::set_param` and `state_changed`
    /// closures can flip it without touching editor internals.
    needs_repaint: Arc<std::sync::atomic::AtomicBool>,
    /// Normalized values captured at the last render pass, in the
    /// same order as `interaction.knob_regions`. Used to detect
    /// host-driven param changes (automation, preset recall) — if any
    /// live value drifts from the last-painted one, we force a
    /// repaint even if the UI never received a direct edit.
    last_painted_values: Vec<f32>,
}

unsafe impl<P: Params> Send for BuiltinEditor<P> {}

impl<P: Params + 'static> BuiltinEditor<P> {
    /// Request a repaint on the next idle tick. Call this if plugin
    /// code mutates display state outside the normal param or
    /// `state_changed` pathways (uncommon). User interaction and
    /// host automation already flag themselves dirty automatically.
    pub fn request_repaint(&self) {
        self.needs_repaint
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn take_needs_repaint(&self) -> bool {
        self.needs_repaint
            .swap(false, std::sync::atomic::Ordering::AcqRel)
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
        self.last_painted_values.clear();
        self.last_painted_values
            .extend(regions.iter().map(|r| r.normalized_value));
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
            #[cfg(target_os = "macos")]
            native: None,
            blit_backend: None,
            needs_repaint: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            last_painted_values: Vec::new(),
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
            #[cfg(target_os = "macos")]
            native: None,
            blit_backend: None,
            needs_repaint: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            last_painted_values: Vec::new(),
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
            #[cfg(target_os = "macos")]
            native: None,
            blit_backend: None,
            needs_repaint: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            last_painted_values: Vec::new(),
        }
    }

    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Render the full UI to the internal CPU pixel buffer.
    pub fn render(&mut self) {
        let (w, h) = (self.layout.width(), self.layout.height());
        let backend = self
            .backend
            .get_or_insert_with(|| CpuBackend::new(w, h).expect("Failed to create backend"));
        // SAFETY: we split the borrow — backend is a separate field from layout/params/etc.
        let backend_ptr = backend as *mut CpuBackend;
        self.render_widgets(unsafe { &mut *backend_ptr });
    }

    /// Render all widgets to any `RenderBackend`.
    ///
    /// Thin wrapper over [`widgets::draw`] that builds a [`ParamSnapshot`]
    /// from the editor's context or fallback params.
    fn render_widgets(&mut self, backend: &mut dyn RenderBackend) {
        // `widgets::draw` does not clear; do it here so the built-in
        // editor's background matches the theme.
        backend.clear(self.theme.background);
        let owned = self.build_snapshot_closures();
        let snapshot = owned.as_snapshot();
        widgets::draw(backend, &self.layout, &self.theme, &snapshot, &mut self.interaction);
    }

    /// Build owned boxed closures from `self.context` / `self.params` that
    /// back a `ParamSnapshot`. Each closure clones the `Arc<P>` or the
    /// `EditorContext`, so `EditorSnapshotClosures` is `'static` and safe
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
                Box::new(move |id| (c.get_param)(id) as f32)
            }
            None => Box::new(move |id| p_get.get_normalized(id).unwrap_or(0.0) as f32),
        };
        let get_param_plain: Box<dyn Fn(u32) -> f32> = match &ctx {
            Some(c) => {
                let c = c.clone();
                Box::new(move |id| (c.get_param_plain)(id) as f32)
            }
            None => Box::new(move |id| p_get_plain.get_plain(id).unwrap_or(0.0) as f32),
        };
        let format_param: Box<dyn Fn(u32) -> String> = match &ctx {
            Some(c) => {
                let c = c.clone();
                Box::new(move |id| (c.format_param)(id))
            }
            None => Box::new(move |id| {
                let v = p_fmt.get_plain(id).unwrap_or(0.0);
                p_fmt.format_value(id, v).unwrap_or_else(|| format!("{:.1}", v))
            }),
        };
        let get_meter: Box<dyn Fn(u32) -> f32> = match &ctx {
            Some(c) => {
                let c = c.clone();
                Box::new(move |id| (c.get_meter)(id))
            }
            None => Box::new(move |_| 0.0),
        };
        let get_options: Box<dyn Fn(u32) -> Vec<String>> = Box::new(move |id| {
            let info = match p_opts.param_infos().into_iter().find(|i| i.id == id) {
                Some(i) => i,
                None => return Vec::new(),
            };
            let count = (info.range.step_count().max(1) as usize) + 1;
            (0..count)
                .map(|i| {
                    let norm = if count <= 1 { 0.0 } else { i as f64 / (count - 1) as f64 };
                    let plain = info.range.denormalize(norm);
                    p_opts.format_value(id, plain).unwrap_or_else(|| format!("{:.0}", plain))
                })
                .collect()
        });
        let default_normalized: Box<dyn Fn(u32) -> f32> = Box::new(move |id| {
            match p_default.param_infos().iter().find(|i| i.id == id) {
                Some(info) => info.range.normalize(info.default_plain) as f32,
                None => 0.0,
            }
        });
        let next_discrete_normalized: Box<dyn Fn(u32) -> f32> = Box::new(move |id| {
            let info = match p_next.param_infos().into_iter().find(|i| i.id == id) {
                Some(i) => i,
                None => return 0.0,
            };
            let plain = p_next.get_plain(id).unwrap_or(0.0);
            let max = info.range.max();
            let next = if plain >= max { 0.0 } else { plain + 1.0 };
            info.range.normalize(next) as f32
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
                    (ctx.begin_edit)(id);
                }
            }
            ParamEdit::Set { id, normalized } => {
                self.params.set_normalized(id, normalized as f64);
                if let Some(ref ctx) = self.context {
                    (ctx.set_param)(id, normalized as f64);
                }
                self.request_repaint();
            }
            ParamEdit::End { id } => {
                if let Some(ref ctx) = self.context {
                    (ctx.end_edit)(id);
                }
            }
        }
    }

    /// Feed a batch of input events through `interaction::dispatch` and
    /// apply the resulting param edits.
    fn dispatch_events(&mut self, events: &[InputEvent]) {
        let hover_before = self.interaction.hover_idx;
        let dd_before = self.interaction.dropdown_is_open();
        let owned = self.build_snapshot_closures();
        let snapshot = owned.as_snapshot();
        let edits = interaction::dispatch(
            events,
            &self.layout,
            &snapshot,
            &mut self.interaction,
        );
        drop(snapshot);
        drop(owned);
        let had_edits = !edits.is_empty();
        for e in edits {
            self.apply_edit(e);
        }
        // Anything that changes a pixel on screen flips the dirty
        // bit: param edits (already covered by `apply_edit`), hover
        // highlights moving between widgets, and dropdown open/close
        // transitions.
        if had_edits
            || self.interaction.hover_idx != hover_before
            || self.interaction.dropdown_is_open() != dd_before
        {
            self.request_repaint();
        }
    }

    /// Get the raw pixel data after rendering (RGBA premultiplied).
    pub fn pixel_data(&self) -> Option<&[u8]> {
        self.backend.as_ref().map(|b| b.data())
    }

    // --- Public API for external backends (truce-gpu) ---

    /// Whether the editor has an active context.
    pub fn has_context(&self) -> bool {
        self.context.is_some()
    }

    /// Take the editor context, leaving `None` in its place.
    /// Used by hot-reload to preserve the context when swapping editors.
    pub fn take_context(&mut self) -> Option<EditorContext> {
        self.context.take()
    }

    /// Set the editor context (host callbacks) without opening the CPU view.
    pub fn set_context(&mut self, context: EditorContext) {
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
        unsafe { update_interaction(self) };
        self.render_widgets(backend);
    }

    // --- Mouse event handlers (public for external backends) ---
    //
    // These thinly wrap `interaction::dispatch` — each converts the call
    // into a 1-element event batch, runs dispatch, and applies the
    // returned `ParamEdit`s via `apply_edit`.

    pub fn on_mouse_down(&mut self, x: f32, y: f32) {
        self.dispatch_events(&[InputEvent::MouseDown {
            x,
            y,
            button: MouseButton::Left,
        }]);
    }

    pub fn on_mouse_dragged(&mut self, x: f32, y: f32) {
        self.dispatch_events(&[InputEvent::MouseMove { x, y }]);
    }

    pub fn on_mouse_up(&mut self, x: f32, y: f32) {
        self.dispatch_events(&[InputEvent::MouseUp {
            x,
            y,
            button: MouseButton::Left,
        }]);
    }

    pub fn on_double_click(&mut self, x: f32, y: f32) {
        self.dispatch_events(&[InputEvent::MouseDoubleClick { x, y }]);
    }

    pub fn on_scroll(&mut self, x: f32, y: f32, delta_y: f32) {
        self.dispatch_events(&[InputEvent::Scroll { x, y, dy: delta_y }]);
    }

    pub fn on_mouse_moved(&mut self, x: f32, y: f32) -> bool {
        self.dispatch_events(&[InputEvent::MouseMove { x, y }]);
        self.interaction.hover_idx.is_some() || self.interaction.dropdown_is_open()
    }
}

// ---------------------------------------------------------------------------
// C callbacks — thin wrappers that cast the context pointer back to &mut Self
// ---------------------------------------------------------------------------

/// Update interaction regions and live param values.
///
/// # Safety
/// The editor must be valid and not concurrently accessed.
pub unsafe fn update_interaction<P: Params + 'static>(editor: &mut BuiltinEditor<P>) {
    match &editor.layout {
        Layout::Rows(pl) => {
            editor.interaction.build_regions(pl);
            let mut flat_idx = 0usize;
            for row in &pl.rows {
                for knob_def in &row.knobs {
                    if let Some(region) = editor.interaction.knob_regions.get_mut(flat_idx) {
                        region.widget_type = resolve_widget_type(
                            knob_def.widget, knob_def.param_id, &*editor.params,
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
                    region.widget_type = resolve_widget_type(
                        gw.widget, gw.param_id, &*editor.params,
                    );
                }
            }
        }
    }
    for region in &mut editor.interaction.knob_regions {
        if let Some(ref ctx) = editor.context {
            region.normalized_value = (ctx.get_param)(region.param_id) as f32;
        } else {
            region.normalized_value =
                editor.params.get_normalized(region.param_id).unwrap_or(0.0) as f32;
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

fn create_wgpu_backend(
    window: &mut baseview::Window,
    phys_w: u32,
    phys_h: u32,
    logical_w: u32,
    logical_h: u32,
) -> BlitBackend {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });

    let surface = unsafe { crate::platform::create_wgpu_surface(&instance, window) }
        .expect("failed to create wgpu surface");

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
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
    let format = caps.formats.iter().find(|f| f.is_srgb()).copied().unwrap_or(caps.formats[0]);

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

    // Blit texture is sized to the CPU pixmap (logical dimensions), not the
    // surface. The shader samples with UV in [0,1] so it stretches to fill
    // whatever the surface size is. This avoids a width/height mismatch
    // between the uploaded bytes and the texture layout.
    let blit = crate::blit::BlitPipeline::new(&device, format, logical_w, logical_h);

    BlitBackend::Wgpu { device, queue, surface, surface_config, blit }
}

enum BlitBackend {
    #[cfg(target_os = "macos")]
    CoreGraphics(crate::cg_blit::CgBlit),
    Wgpu {
        device: wgpu::Device,
        queue: wgpu::Queue,
        surface: wgpu::Surface<'static>,
        surface_config: wgpu::SurfaceConfiguration,
        blit: crate::blit::BlitPipeline,
    },
}

/// Shared ownership of the blit backend between `BuiltinEditor` and the
/// `BuiltinWindowHandler` baseview hands us. Sharing lets the editor
/// drop the wgpu surface *before* it asks baseview to close the NSView
/// — important on AAX where interleaving Metal teardown with baseview's
/// close sequence inside Pro Tools' outer autorelease pool has been
/// seen to leave stale refs in DFW container views.
type SharedBackend = std::sync::Arc<std::sync::Mutex<Option<BlitBackend>>>;

struct BuiltinWindowHandler<P: Params> {
    /// Raw pointer to the BuiltinEditor owned by the host. Valid between
    /// open() and close(). Only accessed from the GUI thread.
    editor: *mut BuiltinEditor<P>,
    backend: SharedBackend,
    scale: f32,
    last_cursor: (f32, f32),
    last_click_time: Option<std::time::Instant>,
    last_click_pos: (f32, f32),
}

// SAFETY: The raw pointer is only accessed from the GUI thread.
// baseview requires Send for WindowHandler.
unsafe impl<P: Params> Send for BuiltinWindowHandler<P> {}

impl<P: Params + 'static> baseview::WindowHandler for BuiltinWindowHandler<P> {
    fn on_frame(&mut self, _window: &mut baseview::Window) {
        let editor = unsafe { &mut *self.editor };

        unsafe { update_interaction(editor) };
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
            let mut guard = match self.backend.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            let Some(ref mut backend) = *guard else {
                // Editor already dropped the backend in its close
                // path. Nothing to do — baseview will tear us down
                // next.
                return;
            };
            match backend {
                #[cfg(target_os = "macos")]
                BlitBackend::CoreGraphics(cg) => {
                    cg.blit(pixels);
                }
                BlitBackend::Wgpu { device, queue, surface, blit, .. } => {
                    blit.update(queue, pixels);
                    let frame = match surface.get_current_texture() {
                        Ok(f) => f,
                        Err(_) => return,
                    };
                    let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let mut encoder = device.create_command_encoder(
                        &wgpu::CommandEncoderDescriptor { label: None },
                    );
                    blit.render(&mut encoder, &view);
                    queue.submit(std::iter::once(encoder.finish()));
                    frame.present();
                }
            }
        }
    }

    fn on_event(
        &mut self,
        _window: &mut baseview::Window,
        event: baseview::Event,
    ) -> baseview::EventStatus {
        match event {
            baseview::Event::Mouse(mouse) => {
                let editor = unsafe { &mut *self.editor };
                match mouse {
                    baseview::MouseEvent::CursorMoved { position, .. } => {
                        // baseview on macOS reports positions in logical
                        // points via `convertPoint:fromView:nil`. Layout
                        // regions are built in the same logical
                        // coordinate space. Do not multiply by scale —
                        // doing so breaks hit-testing on Retina (cursor
                        // at logical 170 compared against a 277-wide
                        // layout was ending up at physical 340, which
                        // sat outside every knob region).
                        let x = position.x as f32;
                        let y = position.y as f32;
                        self.last_cursor = (x, y);
                        editor.on_mouse_moved(x, y);
                    }
                    baseview::MouseEvent::ButtonPressed {
                        button: baseview::MouseButton::Left, ..
                    } => {
                        // WS_CHILD plugin windows don't receive WM_KEYDOWN
                        // until focused; baseview doesn't SetFocus on click,
                        // so we do it here. See truce-egui editor.rs.
                        #[cfg(target_os = "windows")]
                        {
                            if !_window.has_focus() {
                                _window.focus();
                            }
                        }
                        let (x, y) = self.last_cursor;
                        // Double-click detection (300ms, 4px threshold)
                        let now = std::time::Instant::now();
                        let is_double = self.last_click_time.map_or(false, |t| {
                            now.duration_since(t).as_millis() < 300
                                && (x - self.last_click_pos.0).abs() < 4.0
                                && (y - self.last_click_pos.1).abs() < 4.0
                        });
                        self.last_click_time = Some(now);
                        self.last_click_pos = (x, y);

                        if is_double {
                            editor.on_double_click(x, y);
                            self.last_click_time = None; // reset so triple-click doesn't fire
                        } else {
                            editor.on_mouse_down(x, y);
                        }
                    }
                    baseview::MouseEvent::ButtonReleased {
                        button: baseview::MouseButton::Left, ..
                    } => {
                        let (x, y) = self.last_cursor;
                        editor.on_mouse_up(x, y);
                    }
                    baseview::MouseEvent::WheelScrolled { delta, .. } => {
                        let dy = match delta {
                            baseview::ScrollDelta::Lines { y, .. } => y * 10.0,
                            baseview::ScrollDelta::Pixels { y, .. } => y,
                        };
                        let (x, y) = self.last_cursor;
                        editor.on_scroll(x, y, dy);
                    }
                    baseview::MouseEvent::CursorLeft => {
                        editor.on_mouse_moved(-1.0, -1.0);
                    }
                    _ => return baseview::EventStatus::Ignored,
                }
                baseview::EventStatus::Captured
            }
            baseview::Event::Window(baseview::WindowEvent::Resized(info)) => {
                let pw = info.physical_size().width;
                let ph = info.physical_size().height;
                self.scale = info.scale() as f32;
                crate::platform::note_linux_scale_factor(info.scale());
                if let Ok(mut guard) = self.backend.lock() {
                    if let Some(ref mut backend) = *guard {
                        match backend {
                            #[cfg(target_os = "macos")]
                            BlitBackend::CoreGraphics(cg) => {
                                cg.resize(pw, ph);
                            }
                            BlitBackend::Wgpu {
                                device,
                                surface,
                                surface_config,
                                ..
                            } => {
                                surface_config.width = pw;
                                surface_config.height = ph;
                                surface.configure(device, surface_config);
                                // Blit texture stays at pixmap (logical) size — the
                                // shader stretches it to fill the surface.
                            }
                        }
                    }
                }
                baseview::EventStatus::Captured
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
            let param_info = params.param_infos().into_iter()
                .find(|i| i.id == param_id);
            match param_info.as_ref().map(|i| &i.range) {
                Some(truce_params::ParamRange::Discrete { min: 0, max: 1 }) => widgets::WidgetType::Toggle,
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

    fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
        let (w, h) = self.size();
        self.backend = CpuBackend::new(w, h);
        self.context = Some(context);

        // Build interaction regions
        match &self.layout {
            Layout::Rows(pl) => self.interaction.build_regions(pl),
            Layout::Grid(gl) => self.interaction.build_regions_grid(gl),
        }

        // Render initial frame
        self.render();

        let scale = crate::platform::query_backing_scale(&parent);
        let (lw, lh) = (w as f64, h as f64);
        let phys_w = (lw * scale) as u32;
        let phys_h = (lh * scale) as u32;

        // --- Legacy AAX path: native NSView + CgBlit (no baseview) ---
        //
        // Kept as a fallback: the forked baseview at `../baseview`
        // wraps every NSView lifecycle op in an `autoreleasepool!`
        // which should resolve the per-callout ARP crash that
        // originally forced us off baseview on AAX. When that proves
        // stable in Pro Tools this whole branch and the cg_blit /
        // native_view modules can be deleted. Until then, flip
        // `USE_LEGACY_AAX_PATH` below to `true` to revert.
        // Probe: try the forked-baseview path again to see whether
        // dropping wgpu's Surface before baseview's window.close()
        // changes the Pro Tools unload crash.
        #[cfg(target_os = "macos")]
        const USE_LEGACY_AAX_PATH: bool = false;
        #[cfg(target_os = "macos")]
        if USE_LEGACY_AAX_PATH && should_use_cg_blit() {
            let parent_ptr = match parent {
                RawWindowHandle::AppKit(ptr) => ptr,
                _ => std::ptr::null_mut(),
            };
            if !parent_ptr.is_null() {
                let editor_ptr = self as *mut BuiltinEditor<P>;
                let cg_blit = crate::cg_blit::CgBlit::new(
                    std::ptr::null_mut(), // view not available yet; set after open
                    w, h,
                );
                let scale_f32 = scale as f32;
                // Box up context for native view callbacks
                let ctx = Box::new(NativeEditorCtx::<P> {
                    editor: editor_ptr,
                    cg_blit,
                    scale: scale_f32,
                    last_click_time: None,
                    last_click_pos: (0.0, 0.0),
                });
                let ctx_ptr = Box::into_raw(ctx) as *mut std::ffi::c_void;

                let callbacks = crate::native_view::NativeViewCallbacks {
                    ctx: ctx_ptr,
                    on_mouse_moved: native_on_mouse_moved::<P>,
                    on_mouse_dragged: native_on_mouse_dragged::<P>,
                    on_mouse_down: native_on_mouse_down::<P>,
                    on_mouse_up: native_on_mouse_up::<P>,
                    on_scroll: native_on_scroll::<P>,
                    on_mouse_exited: native_on_mouse_exited::<P>,
                    drop_ctx: native_drop_ctx::<P>,
                };

                let native = unsafe {
                    crate::native_view::open(parent_ptr, lw, lh, callbacks)
                };

                // Set up layer-backed view for CgBlit (setContents: path)
                unsafe {
                    use objc::{msg_send, sel, sel_impl};
                    let ns_view = native.ns_view_ptr() as cocoa::base::id;
                    let _: () = msg_send![ns_view, setWantsLayer: cocoa::base::YES];
                    let _: () = msg_send![ns_view, setLayerContentsRedrawPolicy: 0isize];
                }

                // Now update cg_blit with the actual NSView pointer.
                // Use logical size (w, h) — the CPU backend renders at
                // logical resolution. The layer scales to fill the view.
                let ctx = unsafe { &mut *(ctx_ptr as *mut NativeEditorCtx<P>) };
                ctx.cg_blit = crate::cg_blit::CgBlit::new(
                    native.ns_view_ptr(), w, h,
                );

                self.native = Some(native);
                return;
            }
        }


        let options = baseview::WindowOpenOptions {
            title: String::from("truce"),
            size: baseview::Size::new(lw, lh),
            scale: baseview::WindowScalePolicy::SystemScaleFactor,
        };

        let parent_wrapper = crate::platform::ParentWindow(parent);
        let editor_addr = self as *mut BuiltinEditor<P> as usize;
        let scale_f32 = scale as f32;

        // Shared backend cell: the editor keeps one Arc and baseview's
        // window handler gets the other. At close time the editor
        // takes the inner Option and drops it *before* asking baseview
        // to tear down the NSView.
        let shared_backend: SharedBackend =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        self.blit_backend = Some(shared_backend.clone());
        let shared_for_handler = shared_backend;

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut baseview::Window| {
                let mut backend = create_wgpu_backend(window, phys_w, phys_h, w, h);

                // Render + present an initial frame synchronously, before
                // baseview shows the window. Without this, the window briefly
                // displays whatever garbage is in the surface buffer until the
                // first `on_frame` tick — especially noticeable on VST2
                // (Windows), where `effEditOpen` creates and shows the window
                // in one call.
                let editor = unsafe { &mut *(editor_addr as *mut BuiltinEditor<P>) };
                editor.render();
                if let Some(pixels) = editor.pixel_data() {
                    if let BlitBackend::Wgpu { device, queue, surface, blit, .. } = &mut backend {
                        blit.update(queue, pixels);
                        if let Ok(frame) = surface.get_current_texture() {
                            let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
                            let mut encoder = device.create_command_encoder(
                                &wgpu::CommandEncoderDescriptor { label: None },
                            );
                            blit.render(&mut encoder, &view);
                            queue.submit(std::iter::once(encoder.finish()));
                            frame.present();
                        }
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
                    scale: scale_f32,
                    last_cursor: (0.0, 0.0),
                    last_click_time: None,
                    last_click_pos: (0.0, 0.0),
                }
            },
        );

        self.window = Some(window);
    }

    fn close(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(mut native) = self.native.take() {
            native.close();
            self.context = None;
            self.backend = None;
            return;
        }

        // Wrap in autorelease pool so baseview's internal ObjC teardown
        // (removeFromSuperview, etc.) drains autoreleased objects here
        // rather than leaking into the host's pool.
        #[cfg(target_os = "macos")]
        {
            extern "C" {
                fn objc_autoreleasePoolPush() -> *mut std::ffi::c_void;
                fn objc_autoreleasePoolPop(pool: *mut std::ffi::c_void);
            }
            unsafe {
                let pool = objc_autoreleasePoolPush();

                // Probe (1): drop the wgpu Surface (and therefore the
                // CAMetalLayer it owns, the MTLDevice, command queue,
                // etc.) BEFORE asking baseview to release the NSView.
                // If this changes the Pro Tools unload-crash behavior,
                // the Metal teardown sequence is what's leaving stale
                // autoreleased refs in DFW_NSContainer.
                if let Some(shared) = self.blit_backend.take() {
                    if let Ok(mut guard) = shared.lock() {
                        // Drop the backend while the mutex guard holds
                        // exclusive access; the guard itself goes out
                        // of scope right after.
                        drop(guard.take());
                    }
                }

                if let Some(mut window) = self.window.take() {
                    window.close();
                }
                self.context = None;
                self.backend = None;
                objc_autoreleasePoolPop(pool);
            }
            return;
        }

        #[cfg(not(target_os = "macos"))]
        {
            if let Some(mut window) = self.window.take() {
                window.close();
            }
            self.context = None;
            self.backend = None;
        }
    }

    fn idle(&mut self) {
        // Native view: host-driven rendering (no timer).
        #[cfg(target_os = "macos")]
        if let Some(ref native) = self.native {
            // Get the NativeEditorCtx from the view's state ivar
            let ctx_ptr = native.state_ctx();
            if !ctx_ptr.is_null() {
                unsafe {
                    // Wrap in @autoreleasepool so autoreleased objects from
                    // setNeedsDisplay:/drawRect: drain HERE — not in Pro
                    // Tools' per-callout ARP. This is what JUCE does
                    // (JUCE_AUTORELEASEPOOL around every ObjC interaction).
                    extern "C" {
                        fn objc_autoreleasePoolPush() -> *mut std::ffi::c_void;
                        fn objc_autoreleasePoolPop(pool: *mut std::ffi::c_void);
                    }
                    let pool = objc_autoreleasePoolPush();

                    let ctx = &mut *(ctx_ptr as *mut NativeEditorCtx<P>);
                    let editor = &mut *ctx.editor;
                    update_interaction(editor);
                    editor.detect_host_param_changes();
                    // Pro Tools' AAX idle fires at ~30–60 Hz; skipping
                    // the rasterize + blit when nothing changed is the
                    // biggest win for main-thread CPU when multiple
                    // editor windows are open.
                    if editor.take_needs_repaint() {
                        editor.render();
                        editor.stash_painted_values();
                        if let Some(pixels) = editor.pixel_data() {
                            ctx.cg_blit.blit(pixels);
                        }
                    }

                    objc_autoreleasePoolPop(pool);
                }
            }
            return;
        }
        // If no window (standalone/headless), render for external consumption.
        if self.window.is_none() {
            self.render();
        }
    }
}

// ---------------------------------------------------------------------------
// Native view callbacks for AAX (macOS only, no baseview)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
struct NativeEditorCtx<P: Params> {
    editor: *mut BuiltinEditor<P>,
    cg_blit: crate::cg_blit::CgBlit,
    scale: f32,
    last_click_time: Option<std::time::Instant>,
    last_click_pos: (f32, f32),
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn native_on_mouse_moved<P: Params + 'static>(
    ctx: *mut std::ffi::c_void, x: f32, y: f32,
) {
    let ctx = &mut *(ctx as *mut NativeEditorCtx<P>);
    let editor = &mut *ctx.editor;
    // NSView delivers logical points; layout regions are in logical space.
    editor.on_mouse_moved(x, y);
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn native_on_mouse_dragged<P: Params + 'static>(
    ctx: *mut std::ffi::c_void, x: f32, y: f32,
) {
    let ctx = &mut *(ctx as *mut NativeEditorCtx<P>);
    let editor = &mut *ctx.editor;
    editor.on_mouse_dragged(x, y);
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn native_on_mouse_down<P: Params + 'static>(
    ctx: *mut std::ffi::c_void, x: f32, y: f32,
) {
    let ctx = &mut *(ctx as *mut NativeEditorCtx<P>);
    let editor = &mut *ctx.editor;
    // Double-click detection (300ms, 4px threshold)
    let now = std::time::Instant::now();
    let is_double = ctx.last_click_time.map_or(false, |t| {
        now.duration_since(t).as_millis() < 300
            && (x - ctx.last_click_pos.0).abs() < 4.0
            && (y - ctx.last_click_pos.1).abs() < 4.0
    });
    ctx.last_click_time = Some(now);
    ctx.last_click_pos = (x, y);
    if is_double {
        editor.on_double_click(x, y);
        ctx.last_click_time = None;
    } else {
        editor.on_mouse_down(x, y);
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn native_on_mouse_up<P: Params + 'static>(
    ctx: *mut std::ffi::c_void, x: f32, y: f32,
) {
    let ctx = &mut *(ctx as *mut NativeEditorCtx<P>);
    let editor = &mut *ctx.editor;
    editor.on_mouse_up(x, y);
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn native_on_scroll<P: Params + 'static>(
    ctx: *mut std::ffi::c_void, x: f32, y: f32, dy: f32,
) {
    let ctx = &mut *(ctx as *mut NativeEditorCtx<P>);
    let editor = &mut *ctx.editor;
    editor.on_scroll(x, y, dy);
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn native_on_mouse_exited<P: Params + 'static>(
    ctx: *mut std::ffi::c_void,
) {
    let ctx = &mut *(ctx as *mut NativeEditorCtx<P>);
    let editor = &mut *ctx.editor;
    editor.on_mouse_moved(-1.0, -1.0);
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn native_drop_ctx<P: Params>(ctx: *mut std::ffi::c_void) {
    let _ = Box::from_raw(ctx as *mut NativeEditorCtx<P>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{GridLayout, GridWidget, Layout, section, widgets};
    use crate::widgets::WidgetType;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use truce_params::{ParamInfo, ParamRange, ParamFlags, ParamUnit, Params};

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

        fn count(&self) -> usize { 2 }

        fn get_normalized(&self, id: u32) -> Option<f64> {
            self.values.get(id as usize)
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
            Some(format!("{:.0}", value))
        }

        fn parse_value(&self, _id: u32, _text: &str) -> Option<f64> { None }
        fn snap_smoothers(&self) {}
        fn set_sample_rate(&self, _: f64) {}

        fn collect_values(&self) -> (Vec<u32>, Vec<f64>) {
            let ids = vec![0, 1];
            let vals: Vec<f64> = ids.iter().map(|&id| {
                self.get_plain(id).unwrap_or(0.0)
            }).collect();
            (ids, vals)
        }

        fn restore_values(&self, values: &[(u32, f64)]) {
            for &(id, val) in values {
                self.set_plain(id, val);
            }
        }

        fn default_for_gui() -> Self { Self::new() }
    }

    // -- Helpers --

    /// Build a BuiltinEditor with a dropdown at position 0 and a knob at position 1.
    fn make_editor() -> BuiltinEditor<TestParams> {
        let params = Arc::new(TestParams::new());
        let layout = GridLayout::build("TEST", "V0.1", 2, 50.0, vec![widgets(vec![
            GridWidget::dropdown(0u32, "Mode"),
            GridWidget::knob(1u32, "Gain"),
        ])]);
        let mut editor = BuiltinEditor::new_grid(params, layout);
        // Build interaction regions (normally done in open/render)
        if let Layout::Grid(ref gl) = editor.layout {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type = resolve_widget_type(
                        gw.widget, gw.param_id, &*editor.params,
                    );
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
        let layout = GridLayout::build("TEST", "V0.1", 2, 50.0, vec![
            section("SECTION A", vec![
                GridWidget::knob(1u32, "Gain"),
                GridWidget::knob(1u32, "Gain 2"),
            ]),
            section("SECTION B", vec![
                GridWidget::dropdown(0u32, "Mode"),
                GridWidget::knob(1u32, "Gain 3"),
            ]),
        ]);
        let mut editor = BuiltinEditor::new_grid(params, layout);
        if let Layout::Grid(ref gl) = editor.layout {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type = resolve_widget_type(
                        gw.widget, gw.param_id, &*editor.params,
                    );
                }
            }
        }
        editor.render();
        editor
    }

    /// Find the center of the first dropdown widget's region.
    fn dropdown_center(editor: &BuiltinEditor<TestParams>) -> (f32, f32) {
        let region = editor.interaction.knob_regions.iter()
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
        assert!((norm - expected).abs() < 0.01, "expected {expected:.4}, got {norm}");
    }

    // -- Tests: dropdown anchor positioning --

    #[test]
    fn dropdown_anchor_set_after_render() {
        let editor = make_editor();
        let region = editor.interaction.knob_regions.iter()
            .find(|r| r.widget_type == WidgetType::Dropdown)
            .unwrap();

        // Anchor should be within the widget region (below y, above y+h)
        assert!(region.dropdown_anchor_y > region.y,
            "anchor {} should be below region.y {}", region.dropdown_anchor_y, region.y);
        assert!(region.dropdown_anchor_y < region.y + region.h,
            "anchor {} should be above region bottom {}",
            region.dropdown_anchor_y, region.y + region.h);
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

        let r_plain = editor_plain.interaction.knob_regions.iter()
            .find(|r| r.widget_type == WidgetType::Dropdown)
            .unwrap();
        let r_sections = editor_sections.interaction.knob_regions.iter()
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

        fn count(&self) -> usize { 2 }

        fn get_normalized(&self, id: u32) -> Option<f64> {
            self.values.get(id as usize)
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
            Some(format!("{:.0}", value))
        }

        fn parse_value(&self, _id: u32, _text: &str) -> Option<f64> { None }
        fn snap_smoothers(&self) {}
        fn set_sample_rate(&self, _: f64) {}

        fn collect_values(&self) -> (Vec<u32>, Vec<f64>) {
            let ids = vec![0, 1];
            let vals: Vec<f64> = ids.iter().map(|&id| self.get_plain(id).unwrap_or(0.0)).collect();
            (ids, vals)
        }

        fn restore_values(&self, values: &[(u32, f64)]) {
            for &(id, val) in values { self.set_plain(id, val); }
        }

        fn default_for_gui() -> Self { Self::new() }
    }

    // -- Additional helpers --

    /// Build an editor with a dropdown in the last row (near the window bottom).
    fn make_editor_bottom_dropdown() -> BuiltinEditor<TestParams> {
        let params = Arc::new(TestParams::new());
        // 3 rows of 2, dropdown in the last row (row 2)
        let layout = GridLayout::build("TEST", "V0.1", 2, 50.0, vec![widgets(vec![
            GridWidget::knob(1u32, "K1"),
            GridWidget::knob(1u32, "K2"),
            GridWidget::knob(1u32, "K3"),
            GridWidget::knob(1u32, "K4"),
            GridWidget::dropdown(0u32, "Mode"),
            GridWidget::knob(1u32, "K5"),
        ])]);
        let mut editor = BuiltinEditor::new_grid(params, layout);
        if let Layout::Grid(ref gl) = editor.layout {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type = resolve_widget_type(gw.widget, gw.param_id, &*editor.params);
                }
            }
        }
        editor.render();
        editor
    }

    /// Build an editor with two dropdowns side by side.
    fn make_editor_two_dropdowns() -> BuiltinEditor<TestParams> {
        let params = Arc::new(TestParams::new());
        let layout = GridLayout::build("TEST", "V0.1", 2, 50.0, vec![widgets(vec![
            GridWidget::dropdown(0u32, "Mode A"),
            GridWidget::dropdown(0u32, "Mode B"),
        ])]);
        let mut editor = BuiltinEditor::new_grid(params, layout);
        if let Layout::Grid(ref gl) = editor.layout {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type = resolve_widget_type(gw.widget, gw.param_id, &*editor.params);
                }
            }
        }
        editor.render();
        editor
    }

    /// Build an editor with a 20-option dropdown for scroll testing.
    fn make_editor_many_options() -> BuiltinEditor<ManyOptionParams> {
        let params = Arc::new(ManyOptionParams::new());
        let layout = GridLayout::build("TEST", "V0.1", 2, 50.0, vec![widgets(vec![
            GridWidget::dropdown(0u32, "Note"),
            GridWidget::knob(1u32, "Gain"),
        ])]);
        let mut editor = BuiltinEditor::new_grid(params, layout);
        if let Layout::Grid(ref gl) = editor.layout {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type = resolve_widget_type(gw.widget, gw.param_id, &*editor.params);
                }
            }
        }
        editor.render();
        editor
    }

    fn dropdown_center_many(editor: &BuiltinEditor<ManyOptionParams>) -> (f32, f32) {
        let region = editor.interaction.knob_regions.iter()
            .find(|r| r.widget_type == WidgetType::Dropdown)
            .expect("no dropdown in layout");
        (region.x + region.w / 2.0, region.y + region.h / 2.0)
    }

    // -- Tests: dropdown overflow/clipping --

    #[test]
    fn dropdown_flips_upward_when_near_bottom() {
        let mut editor = make_editor_bottom_dropdown();
        let (dx, dy) = {
            let region = editor.interaction.knob_regions.iter()
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
        assert!(dd.options.len() > dd.visible_count,
            "expected scroll: {} options, {} visible", dd.options.len(), dd.visible_count);
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
        assert_eq!(editor.interaction.dropdown.as_ref().unwrap().scroll_offset, 0);

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
            dd.scroll_offset, dd.scroll_offset + dd.visible_count
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
        assert_eq!(editor.interaction.dropdown.as_ref().unwrap().scroll_offset, 3);

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
        assert_eq!(dd.hover_option, Some(last_visible), "expected hover on last visible option");

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
        assert!(px + pw <= window_w + 1.0, "popup right {} > window {window_w}", px + pw);
        assert!(py + ph <= window_h + 1.0, "popup bottom {} > window {window_h}", py + ph);
    }
}
