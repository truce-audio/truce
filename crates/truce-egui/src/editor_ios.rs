//! egui editor on iOS — `CAMetalLayer`-backed `UIView` running
//! egui-wgpu, driven by `CADisplayLink`, with `UITouch` events
//! translated into `egui::RawInput`.
//!
//! Shape mirrors `truce-gui::editor_ios` but with three differences:
//!
//! - The runtime `UIView` subclass overrides `+layerClass` to return
//!   `CAMetalLayer` (so the backing layer is a Metal layer wgpu can
//!   draw into directly). truce-gui's iOS editor uses the default
//!   `CALayer` and blits a tiny-skia pixmap via `CGImage`; egui
//!   needs the GPU path.
//! - Per-frame work runs `egui::Context::run` with a `RawInput`
//!   built from pending touch events + screen size, then hands the
//!   tessellated primitives to `EguiRenderer::render` which presents
//!   the next surface texture.
//! - Touch handlers push `egui::Event::PointerMoved` /
//!   `PointerButton{pressed: bool}` into a shared queue the next
//!   tick drains.

#![cfg(target_os = "ios")]

use std::sync::{Arc, Mutex};

use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, Sel};
use objc2::sel;
use objc2_foundation::{NSPoint, NSRect, NSSize};

use truce_core::editor::{Editor, PluginContext, RawWindowHandle};
use truce_params::Params;

use crate::renderer::EguiRenderer;

pub trait EditorUi<P: Params + ?Sized>: Send {
    fn ui(&mut self, ctx: &egui::Context, state: &PluginContext<P>);
    fn opened(&mut self, _state: &PluginContext<P>) {}
    fn state_changed(&mut self, _state: &PluginContext<P>) {}
}

impl<P: Params + ?Sized, F: FnMut(&egui::Context, &PluginContext<P>) + Send> EditorUi<P> for F {
    fn ui(&mut self, ctx: &egui::Context, state: &PluginContext<P>) {
        self(ctx, state);
    }
}

pub struct EguiEditor<P: Params + ?Sized> {
    params: Arc<P>,
    size: (u32, u32),
    ui: Arc<Mutex<Box<dyn EditorUi<P>>>>,
    visuals: Option<egui::Visuals>,
    font: Option<&'static [u8]>,
    inner: Arc<Mutex<Option<Inner<P>>>>,
}

// SAFETY: see truce-gui::editor_ios for the symmetric rationale —
// UIKit + CADisplayLink + wgpu surface presentation all happen on
// the main thread, where the AUv3 host calls Editor methods.
unsafe impl<P: Params + ?Sized> Send for EguiEditor<P> {}

struct Inner<P: Params + ?Sized> {
    child_view: *mut AnyObject,
    display_link: *mut AnyObject,
    logical_w: u32,
    logical_h: u32,
    scale: f32,
    egui_ctx: egui::Context,
    renderer: EguiRenderer,
    ui: Arc<Mutex<Box<dyn EditorUi<P>>>>,
    params: Arc<P>,
    context: PluginContext<P>,
    pending_events: Vec<egui::Event>,
    last_pointer: egui::Pos2,
}

impl<P: Params + 'static> EguiEditor<P> {
    pub fn new(
        params: Arc<P>,
        size: (u32, u32),
        ui: impl FnMut(&egui::Context, &PluginContext<P>) + Send + 'static,
    ) -> Self {
        Self::with_ui_impl(params, size, Box::new(ui))
    }

    pub fn with_ui_impl(params: Arc<P>, size: (u32, u32), ui: Box<dyn EditorUi<P>>) -> Self {
        Self {
            params,
            size,
            ui: Arc::new(Mutex::new(ui)),
            visuals: None,
            font: None,
            inner: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_ui(params: Arc<P>, size: (u32, u32), ui: impl EditorUi<P> + 'static) -> Self {
        Self::with_ui_impl(params, size, Box::new(ui))
    }

    #[must_use]
    pub fn with_visuals(mut self, visuals: egui::Visuals) -> Self {
        self.visuals = Some(visuals);
        self
    }

    #[must_use]
    pub fn with_font(mut self, font: &'static [u8]) -> Self {
        self.font = Some(font);
        self
    }
}

impl<P: Params + 'static> Editor for EguiEditor<P> {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        let RawWindowHandle::UiKit(parent_ptr) = parent else {
            log::warn!("EguiEditor (iOS) got non-UiKit parent handle");
            return;
        };
        if parent_ptr.is_null() {
            return;
        }
        let (lw, lh) = self.size;
        // `query_backing_scale(parent)` reads `parent.contentScaleFactor`
        // — but at `gui_open` time the AUv3 container UIView hasn't
        // been attached to a visible window yet, so the property
        // still returns its default 1.0 instead of the device's
        // actual scale. `main_screen_scale()` goes through
        // `UIScreen.mainScreen.scale` and returns 3.0 on iPhone
        // Retina regardless of the view hierarchy state. Without
        // this, egui paints into a 1× wgpu surface that
        // CoreAnimation upscales 3× — visibly grainy edges.
        // Mirrors the built-in iOS editor's `EditorScale::new(...)`
        // construction.
        let scale = truce_gui::platform::main_screen_scale();
        // Physical-pixel math bounded by editor size × backing
        // scale (max ~4000 px in practice); the cast loss is
        // irrelevant. `scale` won't exceed 4.0 on any Apple device.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let scalef = scale as f32;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let phys_w = (f64::from(lw) * scale).round() as u32;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let phys_h = (f64::from(lh) * scale).round() as u32;

        // Create the runtime UIView subclass + CAMetalLayer, attach
        // to the parent, return (view*, layer*, link*).
        // SAFETY: see install_editor_view's contract.
        let (view, layer, link, slot_ptr) =
            unsafe { install_editor_view::<P>(parent_ptr.cast(), lw, lh, scalef, &self.inner) };
        if view.is_null() || layer.is_null() {
            log::warn!("egui iOS: install_editor_view returned null");
            return;
        }

        // Build the egui-wgpu renderer from the metal layer.
        // SAFETY: layer outlives the renderer (held by the view, view
        // pinned via ivar Arc).
        let Some(renderer) =
            (unsafe { EguiRenderer::from_metal_layer(layer.cast(), phys_w, phys_h) })
        else {
            log::warn!("egui iOS: failed to create EguiRenderer from metal layer");
            unsafe {
                let _: () = msg_send![view, removeFromSuperview];
            }
            return;
        };

        let egui_ctx = egui::Context::default();
        // Pin egui's logical→physical scale to the device backing
        // scale (3× on Retina iPhones). Without this, egui paints
        // at 1× pixels-per-point into a 3×-sized wgpu surface and
        // Core Animation upscales the result — visible as grainy
        // edges on every widget. Mirrors the macOS editor's
        // backingScaleFactor application.
        egui_ctx.set_pixels_per_point(scalef);
        if let Some(v) = self.visuals.clone() {
            egui_ctx.set_visuals(v);
        }
        if let Some(font_bytes) = self.font {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "truce-egui".into(),
                Arc::new(egui::FontData::from_static(font_bytes)),
            );
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "truce-egui".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "truce-egui".into());
            egui_ctx.set_fonts(fonts);
        }

        let typed_ctx = context.with_params(Arc::clone(&self.params));
        if let Ok(mut ui) = self.ui.lock() {
            ui.opened(&typed_ctx);
        }

        let inner = Inner {
            child_view: view,
            display_link: link,
            logical_w: lw,
            logical_h: lh,
            scale: scalef,
            egui_ctx,
            renderer,
            ui: Arc::clone(&self.ui),
            params: Arc::clone(&self.params),
            context: typed_ctx,
            pending_events: Vec::with_capacity(16),
            last_pointer: egui::pos2(-1.0, -1.0),
        };
        *self.inner.lock().expect("inner mutex") = Some(inner);
        let _ = slot_ptr; // pin already taken into the ivar
    }

    fn close(&mut self) {
        let Some(inner) = self.inner.lock().expect("inner mutex").take() else {
            return;
        };
        unsafe {
            if !inner.display_link.is_null() {
                let _: () = msg_send![inner.display_link, invalidate];
                let _: () = msg_send![inner.display_link, release];
            }
            if !inner.child_view.is_null() {
                // Reclaim the Arc the view's ivar holds.
                let cls: &AnyClass = msg_send![inner.child_view, class];
                let base: *const u8 = inner.child_view.cast();
                let ivar_ptr: *const *mut std::ffi::c_void =
                    base.add(ivar_offset(cls, INNER_PTR_IVAR)).cast();
                let leaked = (*ivar_ptr).cast_const().cast::<Mutex<Option<Inner<P>>>>();
                if !leaked.is_null() {
                    let _ = Arc::from_raw(leaked);
                }
                let _: () = msg_send![inner.child_view, removeFromSuperview];
            }
        }
        // EguiRenderer drops here, releasing wgpu surface / device / queue.
        drop(inner);
    }
}

// ---------------------------------------------------------------------------
// UIView subclass with CAMetalLayer + CADisplayLink + touch handlers
// ---------------------------------------------------------------------------

const INNER_PTR_IVAR: &std::ffi::CStr = c"_truce_egui_inner_ptr";

unsafe extern "C" {
    static NSRunLoopCommonModes: *const AnyObject;
}

/// `+[Class layerClass]` override that returns `CAMetalLayer`. Class
/// method (not instance), takes only `self` and `_cmd`; the return
/// is an `objc_class *`.
unsafe extern "C" fn layer_class_thunk(_cls: &AnyClass, _cmd: Sel) -> *const AnyClass {
    AnyClass::get(c"CAMetalLayer").expect("CAMetalLayer missing")
}

unsafe fn install_editor_view<P: Params + 'static>(
    parent: *mut AnyObject,
    logical_w: u32,
    logical_h: u32,
    scale: f32,
    slot: &Arc<Mutex<Option<Inner<P>>>>,
) -> (
    *mut AnyObject,
    *mut AnyObject,
    *mut AnyObject,
    *const Mutex<Option<Inner<P>>>,
) {
    use std::any::type_name;
    unsafe {
        let class_name_owned = format!(
            "TruceEguiiOSEditorView_{:x}",
            seahash(type_name::<Inner<P>>().as_bytes())
        );
        let class_name = std::ffi::CString::new(class_name_owned).expect("ascii");
        let uiview = AnyClass::get(c"UIView").expect("UIView missing");

        let cls: &AnyClass = if let Some(existing) = AnyClass::get(class_name.as_c_str()) {
            existing
        } else {
            let mut builder = ClassBuilder::new(class_name.as_c_str(), uiview)
                .expect("unique class name per monomorphization");
            builder.add_ivar::<*mut std::ffi::c_void>(INNER_PTR_IVAR);
            // `+layerClass` returns the class that backs every
            // instance's `layer` property. Returning `CAMetalLayer`
            // here makes `self.layer` a CAMetalLayer directly, so
            // wgpu can draw into it without manual sublayer
            // attachment + resize bookkeeping.
            builder.add_class_method(
                sel!(layerClass),
                layer_class_thunk as unsafe extern "C" fn(_, _) -> _,
            );
            builder.add_method(
                sel!(tick:),
                tick_thunk::<P> as unsafe extern "C" fn(_, _, _),
            );
            builder.add_method(
                sel!(touchesBegan:withEvent:),
                touches_began::<P> as unsafe extern "C" fn(_, _, _, _),
            );
            builder.add_method(
                sel!(touchesMoved:withEvent:),
                touches_moved::<P> as unsafe extern "C" fn(_, _, _, _),
            );
            builder.add_method(
                sel!(touchesEnded:withEvent:),
                touches_ended::<P> as unsafe extern "C" fn(_, _, _, _),
            );
            builder.add_method(
                sel!(touchesCancelled:withEvent:),
                touches_cancelled::<P> as unsafe extern "C" fn(_, _, _, _),
            );
            builder.register()
        };

        let frame = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: f64::from(logical_w),
                height: f64::from(logical_h),
            },
        };
        let alloc: *mut AnyObject = msg_send![cls, alloc];
        let view: *mut AnyObject = msg_send![alloc, initWithFrame: frame];
        if view.is_null() {
            return (
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
        }
        let _: () = msg_send![view, setUserInteractionEnabled: true];
        let _: () = msg_send![view, setContentScaleFactor: f64::from(scale)];

        // Reach into the view's layer (already a CAMetalLayer thanks
        // to +layerClass) and configure its drawable scale + size.
        let layer: *mut AnyObject = msg_send![view, layer];
        let _: () = msg_send![layer, setContentsScale: f64::from(scale)];
        let drawable_size = NSSize {
            width: f64::from(logical_w) * f64::from(scale),
            height: f64::from(logical_h) * f64::from(scale),
        };
        let _: () = msg_send![layer, setDrawableSize: drawable_size];

        // Pin the Arc into the ivar (released in close() via
        // `Arc::from_raw`).
        let leaked: *const Mutex<Option<Inner<P>>> = Arc::into_raw(Arc::clone(slot));
        let base = view.cast::<u8>();
        let ivar_ptr: *mut *mut std::ffi::c_void =
            base.add(ivar_offset(cls, INNER_PTR_IVAR)).cast();
        *ivar_ptr = leaked as *mut std::ffi::c_void;

        let _: () = msg_send![parent, addSubview: view];

        // CADisplayLink → tick: every refresh.
        let dl_cls = AnyClass::get(c"CADisplayLink").expect("CADisplayLink missing");
        let link: *mut AnyObject =
            msg_send![dl_cls, displayLinkWithTarget: view, selector: sel!(tick:)];
        if link.is_null() {
            return (view, layer, std::ptr::null_mut(), leaked);
        }
        let _: () = msg_send![link, retain];
        let run_loop_cls = AnyClass::get(c"NSRunLoop").expect("NSRunLoop missing");
        let main: *mut AnyObject = msg_send![run_loop_cls, mainRunLoop];
        let mode: *const AnyObject = NSRunLoopCommonModes;
        let _: () = msg_send![link, addToRunLoop: main, forMode: mode];

        (view, layer, link, leaked)
    }
}

unsafe fn borrow_inner_arc<P: Params + 'static>(
    self_: &AnyObject,
) -> Option<Arc<Mutex<Option<Inner<P>>>>> {
    unsafe {
        let cls: &AnyClass = msg_send![self_, class];
        let base: *const u8 = std::ptr::from_ref::<AnyObject>(self_).cast();
        let ivar_ptr: *const *mut std::ffi::c_void =
            base.add(ivar_offset(cls, INNER_PTR_IVAR)).cast();
        let leaked = (*ivar_ptr).cast_const().cast::<Mutex<Option<Inner<P>>>>();
        if leaked.is_null() {
            return None;
        }
        let arc = Arc::from_raw(leaked);
        let cloned = Arc::clone(&arc);
        let _ = Arc::into_raw(arc);
        Some(cloned)
    }
}

unsafe extern "C" fn tick_thunk<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    _sender: *mut AnyObject,
) {
    unsafe {
        let Some(arc) = borrow_inner_arc::<P>(self_) else {
            return;
        };
        let Ok(mut guard) = arc.lock() else { return };
        let Some(inner) = guard.as_mut() else { return };
        run_frame(inner);
    }
}

fn run_frame<P: Params + 'static>(inner: &mut Inner<P>) {
    // `as f32` from u32: editor logical dimensions stay well below
    // 2^23, so the f32 mantissa loss never matters.
    #[allow(clippy::cast_precision_loss)]
    let screen_rect = egui::Rect::from_min_size(
        egui::pos2(0.0, 0.0),
        egui::vec2(inner.logical_w as f32, inner.logical_h as f32),
    );
    let mut raw_input = egui::RawInput {
        screen_rect: Some(screen_rect),
        time: Some(timestamp_seconds()),
        ..Default::default()
    };
    raw_input.events = std::mem::take(&mut inner.pending_events);
    // Pin the root viewport's pixels_per_point on every frame.
    // `RawInput::default()` seeds the viewport map with
    // `native_pixels_per_point: None`, which egui then resolves to
    // 1.0 — overriding `Context::set_pixels_per_point` we ran at
    // open. Without this, every frame's tessellation snaps back to
    // 1× DPI and Core Animation upscales the result, producing the
    // grainy edges we saw.
    raw_input
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .native_pixels_per_point = Some(inner.scale);

    let output = inner.egui_ctx.run(raw_input, |ctx| {
        if let Ok(mut ui) = inner.ui.lock() {
            ui.ui(ctx, &inner.context);
        }
    });
    let clipped = inner
        .egui_ctx
        .tessellate(output.shapes, output.pixels_per_point);
    inner
        .renderer
        .render(&output.textures_delta, &clipped, inner.scale);
    let _ = inner.params;
}

fn timestamp_seconds() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

unsafe extern "C" fn touches_began<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe { dispatch_touch::<P>(self_, touches, TouchPhase::Began) }
}

unsafe extern "C" fn touches_moved<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe { dispatch_touch::<P>(self_, touches, TouchPhase::Moved) }
}

unsafe extern "C" fn touches_ended<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe { dispatch_touch::<P>(self_, touches, TouchPhase::Ended) }
}

unsafe extern "C" fn touches_cancelled<P: Params + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe { dispatch_touch::<P>(self_, touches, TouchPhase::Ended) }
}

#[derive(Clone, Copy)]
enum TouchPhase {
    Began,
    Moved,
    Ended,
}

unsafe fn dispatch_touch<P: Params + 'static>(
    self_: &AnyObject,
    touches: *mut AnyObject,
    phase: TouchPhase,
) {
    unsafe {
        let Some(arc) = borrow_inner_arc::<P>(self_) else {
            return;
        };
        let Ok(mut guard) = arc.lock() else { return };
        let Some(inner) = guard.as_mut() else { return };

        // Pick one touch — egui is single-pointer. Real multi-touch
        // would need an `egui::Event` per finger which egui doesn't
        // model natively; "first finger wins" matches what JUCE and
        // the truce built-in editor do.
        let touch: *mut AnyObject = msg_send![touches, anyObject];
        if touch.is_null() {
            return;
        }
        let view_ptr: *mut AnyObject = std::ptr::from_ref::<AnyObject>(self_).cast_mut();
        let pt: NSPoint = msg_send![touch, locationInView: view_ptr];
        #[allow(clippy::cast_possible_truncation)]
        let pos = egui::pos2(pt.x as f32, pt.y as f32);
        inner.last_pointer = pos;
        inner.pending_events.push(egui::Event::PointerMoved(pos));
        match phase {
            TouchPhase::Began => {
                inner.pending_events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                });
            }
            TouchPhase::Ended => {
                inner.pending_events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                });
                inner.pending_events.push(egui::Event::PointerGone);
            }
            TouchPhase::Moved => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Utilities — lifted from truce-gui::editor_ios (kept duplicated; both
// crates need them, and the helper is six lines).
// ---------------------------------------------------------------------------

fn seahash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

unsafe fn ivar_offset(cls: &AnyClass, name: &std::ffi::CStr) -> usize {
    unsafe extern "C" {
        fn class_getInstanceVariable(
            cls: *const AnyClass,
            name: *const std::os::raw::c_char,
        ) -> *mut std::ffi::c_void;
        fn ivar_getOffset(ivar: *mut std::ffi::c_void) -> isize;
    }
    unsafe {
        let ivar = class_getInstanceVariable(std::ptr::from_ref::<AnyClass>(cls), name.as_ptr());
        assert!(!ivar.is_null(), "ivar {name:?} not registered");
        let off = ivar_getOffset(ivar);
        usize::try_from(off).expect("non-negative ivar offset")
    }
}
