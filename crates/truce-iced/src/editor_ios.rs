//! iced editor on iOS: a `CAMetalLayer`-backed `UIView` running the
//! shared [`IcedRuntime`] pipeline, driven by `CADisplayLink`, with
//! `UITouch` events translated into iced mouse events.
//!
//! The desktop editor (`editor.rs`) drives the same `IcedRuntime` from a
//! baseview window. Here the runtime instead renders into a Metal layer
//! wgpu draws onto directly: the runtime `UIView` subclass overrides
//! `+layerClass` to return `CAMetalLayer`, so `self.layer` is a Metal
//! layer the iced wgpu renderer can present into. Each `CADisplayLink`
//! tick runs `IcedRuntime::tick`; touch handlers push
//! `mouse::Event::CursorMoved` / `ButtonPressed` / `ButtonReleased` into
//! the runtime's pending-event queue the next tick drains.
//!
//! Text input: the view conforms to `UIKeyInput`, and each tick mirrors
//! the UI's `InputMethod` state into `becomeFirstResponder` /
//! `resignFirstResponder` so the soft keyboard rises when an iced
//! `text_input` is focused. Typed characters and backspace arrive via
//! `insertText:` / `deleteBackward` and become iced keyboard events.

#![cfg(target_os = "ios")]

use std::sync::{Arc, Mutex};

use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, Bool, ClassBuilder, Sel};
use objc2::sel;
use objc2_foundation::{NSPoint, NSRect, NSSize};

use iced_wgpu::wgpu;
use truce_core::editor::{Editor, PluginContext, RawWindowHandle};
use truce_gui::EditorScale;
use truce_gui::ios::{TouchPhase, fnv1a_64, ivar_offset};
use truce_gui::layout::GridLayout;
use truce_params::Params;

use crate::iced::keyboard::key::{Named, NativeCode, Physical};
use crate::iced::keyboard::{Key, Location, Modifiers};
use crate::iced::{Event, keyboard, mouse};
use crate::param_cache::ParamCache;
use crate::runtime::{AutoPlugin, IcedPlugin, IcedProgram, IcedRuntime};

/// iced-based plugin editor (iOS). Mirrors the desktop `IcedEditor`
/// builder surface so plugin code stays portable; the windowing host is
/// a `CAMetalLayer` `UIView` instead of a baseview window.
pub struct IcedEditor<P, M>
where
    P: Params + 'static,
    M: IcedPlugin<P>,
{
    params: Arc<P>,
    size: (u32, u32),
    /// Resize-capability flag exposed via `Editor::can_resize`. The
    /// AU v3 view controller only fits the editor to the host's
    /// safe-area frame when this is `true`; the default keeps a
    /// fixed-size GUI pinned to its built size.
    can_resize: bool,
    /// `Editor::min_size` / `max_size` bounds. The AU shim clamps
    /// host-driven resizes against these before calling `set_size`.
    min_size: (u32, u32),
    max_size: (u32, u32),
    /// `Editor::aspect_ratio` lock (numerator, denominator). The AU
    /// shim's `fit_logical_size` clamps host-driven resizes to it.
    aspect_ratio: Option<(u32, u32)>,
    font: Option<&'static [u8]>,
    make_plugin: Box<dyn Fn(Arc<P>) -> M + Send + Sync>,
    meter_ids: Vec<u32>,
    inner: InnerSlot<P, M>,
}

/// Shared, mutable slot holding the live editor state. Pinned into the
/// `UIView`'s ivar as a raw `Arc` while the editor is open; the tick and
/// touch callbacks borrow it back out by type.
type InnerSlot<P, M> = Arc<Mutex<Option<Inner<P, M>>>>;

// SAFETY: UIKit + CADisplayLink + wgpu surface presentation all happen on
// the main thread, where the AUv3 host calls Editor methods. The editor
// never crosses threads; `Send` is required only to live behind the
// `dyn Editor` trait object.
unsafe impl<P: Params, M: IcedPlugin<P>> Send for IcedEditor<P, M> {}

struct Inner<P: Params + 'static, M: IcedPlugin<P>> {
    child_view: *mut AnyObject,
    display_link: *mut AnyObject,
    runtime: IcedRuntime<P, M>,
}

impl<P: Params + 'static> IcedEditor<P, AutoPlugin> {
    /// Create an editor that auto-generates the UI from a `GridLayout`.
    pub fn from_layout(params: Arc<P>, layout: GridLayout) -> Self {
        let size = (layout.width, layout.height);
        let meter_ids: Vec<u32> = layout
            .widgets
            .iter()
            .filter_map(|w| w.meter_ids.as_ref())
            .flatten()
            .copied()
            .collect();
        let make_plugin: Box<dyn Fn(Arc<P>) -> AutoPlugin + Send + Sync> =
            Box::new(move |_params| AutoPlugin {
                layout: layout.clone(),
            });
        Self {
            params,
            size,
            can_resize: false,
            min_size: (1, 1),
            max_size: (u32::MAX, u32::MAX),
            aspect_ratio: None,
            font: None,
            make_plugin,
            meter_ids,
            inner: Arc::new(Mutex::new(None)),
        }
    }
}

impl<P: Params + 'static, M: IcedPlugin<P> + 'static> IcedEditor<P, M> {
    /// Create an editor with a custom `IcedPlugin` implementation.
    pub fn new(params: Arc<P>, size: (u32, u32)) -> Self {
        Self {
            params,
            size,
            can_resize: false,
            min_size: (1, 1),
            max_size: (u32::MAX, u32::MAX),
            aspect_ratio: None,
            font: None,
            make_plugin: Box::new(|p| M::new(p)),
            meter_ids: Vec::new(),
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// Set a custom default font (TTF bytes).
    #[must_use]
    pub fn with_font(mut self, data: &'static [u8]) -> Self {
        self.font = Some(data);
        self
    }

    /// Set meter IDs to poll each tick.
    #[must_use]
    pub fn with_meter_ids(mut self, ids: Vec<impl Into<u32>>) -> Self {
        self.meter_ids = ids.into_iter().map(Into::into).collect();
        self
    }

    /// Opt into host-driven resizing. When `true`, the AU v3 view
    /// controller fits the editor to the host plug-in pane's safe-area
    /// frame (driving `set_size` through the AU shim), so the editor
    /// reflows to the real device viewport instead of sitting at its
    /// built size. The default (`false`) keeps a fixed-size GUI pinned.
    /// Mirrors the desktop `IcedEditor::resizable` so the same builder
    /// call works on every target.
    #[must_use]
    pub fn resizable(mut self, resizable: bool) -> Self {
        self.can_resize = resizable;
        self
    }

    /// No-op on iOS. See [`Self::resizable`].
    #[must_use]
    pub fn maximizable(self, _maximizable: bool) -> Self {
        self
    }

    /// Minimum logical-point size the editor accepts. The AU shim
    /// consults this before driving `set_size`. See [`Self::resizable`].
    #[must_use]
    pub fn min_size(mut self, min: (u32, u32)) -> Self {
        self.min_size = min;
        self
    }

    /// Maximum logical-point size the editor accepts. See
    /// [`Self::min_size`].
    #[must_use]
    pub fn max_size(mut self, max: (u32, u32)) -> Self {
        self.max_size = max;
        self
    }

    /// Lock the editor's aspect ratio as `(numerator, denominator)`.
    /// Host-driven resizes are clamped to it by the AU shim's
    /// `fit_logical_size`. See [`Self::resizable`].
    #[must_use]
    pub fn aspect_ratio(mut self, ratio: Option<(u32, u32)>) -> Self {
        self.aspect_ratio = ratio;
        self
    }

    /// No-op on iOS. See [`Self::resizable`].
    #[must_use]
    pub fn prefers_pow2(self, _prefers: bool) -> Self {
        self
    }

    /// Build the plugin model + iced program, wrapped in an `IcedRuntime`.
    fn build_runtime(&self, context: &PluginContext) -> IcedRuntime<P, M> {
        let plugin = (self.make_plugin)(self.params.clone());
        let mut param_cache = ParamCache::new(self.params.clone());
        if let Some(data) = self.font {
            param_cache.set_font(crate::font::apply_font(data));
        }
        let program = IcedProgram {
            plugin,
            param_cache,
            context: context.with_params(self.params.clone()),
            meter_ids: self.meter_ids.clone(),
        };
        // The AUv3 container owns sizing; pin the runtime's logical->
        // physical scale to the device backing scale (3x on Retina).
        let scale = EditorScale::new(truce_gui::platform::main_screen_scale());
        IcedRuntime::new(self.size, scale, self.font, program)
    }
}

impl<P: Params + 'static, M: IcedPlugin<P> + 'static> Editor for IcedEditor<P, M> {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        let RawWindowHandle::UiKit(parent_ptr) = parent else {
            log::warn!("IcedEditor (iOS) got non-UiKit parent handle");
            return;
        };
        if parent_ptr.is_null() {
            return;
        }

        let mut runtime = self.build_runtime(&context);
        let (lw, lh) = self.size;
        let scale = truce_gui::platform::main_screen_scale();

        // Create the runtime UIView subclass + CAMetalLayer, attach to
        // the parent, return (view*, layer*, link*).
        // SAFETY: see install_editor_view's contract.
        let (view, layer, link) =
            unsafe { install_editor_view::<P, M>(parent_ptr.cast(), lw, lh, scale, &self.inner) };
        if view.is_null() || layer.is_null() {
            log::warn!("iced iOS: install_editor_view returned null");
            return;
        }

        // Metal-backed wgpu instance + surface from the layer; the shared
        // `init_render` requests the adapter/device and configures the
        // surface at physical size from the runtime's logical size x scale.
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });
        // SAFETY: `layer` is a CAMetalLayer owned by `view`, which is
        // pinned via the ivar Arc for the editor's lifetime.
        let surface = unsafe {
            instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(layer.cast()))
        };
        match surface {
            Ok(surface) => {
                runtime.init_render(instance, surface);
            }
            Err(e) => {
                log::warn!("iced iOS: create_surface_unsafe failed: {e}");
                unsafe {
                    let _: () = msg_send![view, removeFromSuperview];
                }
                return;
            }
        }

        let inner = Inner {
            child_view: view,
            display_link: link,
            runtime,
        };
        *self.inner.lock().expect("inner mutex") = Some(inner);
        log::info!("iced editor opened on iOS ({lw}x{lh})");
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
                let leaked = (*ivar_ptr)
                    .cast_const()
                    .cast::<Mutex<Option<Inner<P, M>>>>();
                if !leaked.is_null() {
                    let _ = Arc::from_raw(leaked);
                }
                let _: () = msg_send![inner.child_view, removeFromSuperview];
            }
        }
        // IcedRuntime drops here, releasing wgpu surface / device / queue.
        drop(inner);
        log::info!("iced editor closed on iOS");
    }

    fn idle(&mut self) {
        // CADisplayLink drives the frame loop via tick:.
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        self.size = (width, height);
        // The AUv3 view controller calls `gui_set_size` on the main
        // thread from `viewDidLayoutSubviews`, the same thread the
        // `CADisplayLink` runs `tick` on, so resizing the live view +
        // surface inline here is safe (the tick and this never nest -
        // they take the same `inner` mutex on the same thread).
        if let Some(inner) = self.inner.lock().expect("inner mutex").as_mut() {
            resize_inner(inner, width, height);
        }
        true
    }

    fn can_resize(&self) -> bool {
        self.can_resize
    }

    fn min_size(&self) -> (u32, u32) {
        self.min_size
    }

    fn max_size(&self) -> (u32, u32) {
        self.max_size
    }

    fn aspect_ratio(&self) -> Option<(u32, u32)> {
        self.aspect_ratio
    }
}

/// Resize the live editor surface to `logical_w` x `logical_h` logical
/// points. Updates the `UIView` frame, the `CAMetalLayer` drawable (in
/// physical pixels), and the runtime's cached logical size + wgpu
/// surface + iced viewport. The next tick reflows iced against the new
/// viewport. No-op when the size is unchanged so redundant layout passes
/// are cheap.
fn resize_inner<P: Params + 'static, M: IcedPlugin<P> + 'static>(
    inner: &mut Inner<P, M>,
    logical_w: u32,
    logical_h: u32,
) {
    if logical_w == 0 || logical_h == 0 {
        return;
    }
    if inner.runtime.size == (logical_w, logical_h) {
        return;
    }

    let render_scale = inner.runtime.scale.get();
    let phys_w = truce_gui::to_physical_px(logical_w, render_scale).max(1);
    let phys_h = truce_gui::to_physical_px(logical_h, render_scale).max(1);

    let frame = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            width: f64::from(logical_w),
            height: f64::from(logical_h),
        },
    };

    // SAFETY: `child_view` is the pinned UIView (its `layer` is the
    // CAMetalLayer wgpu draws into); both outlive `inner`. Frame /
    // drawable updates are main-thread UIKit calls, which is where
    // `set_size` runs.
    unsafe {
        let _: () = msg_send![inner.child_view, setFrame: frame];
        let layer: *mut AnyObject = msg_send![inner.child_view, layer];
        let drawable_size = NSSize {
            width: f64::from(phys_w),
            height: f64::from(phys_h),
        };
        let _: () = msg_send![layer, setDrawableSize: drawable_size];
    }

    // Reconfigure the wgpu surface + rebuild the iced viewport against
    // the new logical size.
    inner.runtime.resize(logical_w, logical_h);
}

// UIView subclass with CAMetalLayer + CADisplayLink + touch handlers

const INNER_PTR_IVAR: &std::ffi::CStr = c"_truce_iced_inner_ptr";

unsafe extern "C" {
    static NSRunLoopCommonModes: *const AnyObject;
}

/// `+[Class layerClass]` override returning `CAMetalLayer`.
unsafe extern "C" fn layer_class_thunk(_cls: &AnyClass, _cmd: Sel) -> *const AnyClass {
    AnyClass::get(c"CAMetalLayer").expect("CAMetalLayer missing")
}

unsafe fn install_editor_view<P: Params + 'static, M: IcedPlugin<P> + 'static>(
    parent: *mut AnyObject,
    logical_w: u32,
    logical_h: u32,
    scale: f64,
    slot: &InnerSlot<P, M>,
) -> (*mut AnyObject, *mut AnyObject, *mut AnyObject) {
    use std::any::type_name;
    unsafe {
        let class_name_owned = format!(
            "TruceIcediOSEditorView_{:x}",
            fnv1a_64(type_name::<Inner<P, M>>().as_bytes())
        );
        let class_name = std::ffi::CString::new(class_name_owned).expect("ascii");
        let uiview = AnyClass::get(c"UIView").expect("UIView missing");

        let cls: &AnyClass = if let Some(existing) = AnyClass::get(class_name.as_c_str()) {
            existing
        } else {
            let mut builder = ClassBuilder::new(class_name.as_c_str(), uiview)
                .expect("unique class name per monomorphization");
            builder.add_ivar::<*mut std::ffi::c_void>(INNER_PTR_IVAR);
            // `+layerClass` returning `CAMetalLayer` makes `self.layer` a
            // Metal layer wgpu can draw into without a manual sublayer.
            builder.add_class_method(
                sel!(layerClass),
                layer_class_thunk as unsafe extern "C" fn(_, _) -> _,
            );
            builder.add_method(
                sel!(tick:),
                tick_thunk::<P, M> as unsafe extern "C" fn(_, _, _),
            );
            builder.add_method(
                sel!(touchesBegan:withEvent:),
                touches_began::<P, M> as unsafe extern "C" fn(_, _, _, _),
            );
            builder.add_method(
                sel!(touchesMoved:withEvent:),
                touches_moved::<P, M> as unsafe extern "C" fn(_, _, _, _),
            );
            builder.add_method(
                sel!(touchesEnded:withEvent:),
                touches_ended::<P, M> as unsafe extern "C" fn(_, _, _, _),
            );
            builder.add_method(
                sel!(touchesCancelled:withEvent:),
                touches_cancelled::<P, M> as unsafe extern "C" fn(_, _, _, _),
            );
            // `UIKeyInput` conformance for the soft keyboard. UIKit calls
            // `respondsToSelector:` before delivering text, and only
            // presents the keyboard for a first responder that conforms
            // to `UIKeyInput`. `canBecomeFirstResponder` overrides
            // `UIResponder`'s default `NO` so `becomeFirstResponder`
            // (driven each tick from the UI's `InputMethod` state) takes.
            builder.add_method(
                sel!(canBecomeFirstResponder),
                can_become_first_responder as unsafe extern "C" fn(_, _) -> Bool,
            );
            builder.add_method(
                sel!(hasText),
                has_text as unsafe extern "C" fn(_, _) -> Bool,
            );
            builder.add_method(
                sel!(insertText:),
                insert_text::<P, M> as unsafe extern "C" fn(_, _, _),
            );
            builder.add_method(
                sel!(deleteBackward),
                delete_backward::<P, M> as unsafe extern "C" fn(_, _),
            );
            // UIKit checks `conformsToProtocol:` before presenting the
            // keyboard; implementing the selectors isn't enough.
            // `UITextInputTraits` (which `UIKeyInput` inherits) gets
            // sensible defaults from empty trait methods.
            if let Some(proto) = AnyProtocol::get(c"UIKeyInput") {
                builder.add_protocol(proto);
            }
            if let Some(proto) = AnyProtocol::get(c"UITextInputTraits") {
                builder.add_protocol(proto);
            }
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
            );
        }
        let _: () = msg_send![view, setUserInteractionEnabled: true];
        let _: () = msg_send![view, setContentScaleFactor: scale];

        // Configure the CAMetalLayer's drawable scale + physical size.
        let layer: *mut AnyObject = msg_send![view, layer];
        let _: () = msg_send![layer, setContentsScale: scale];
        let drawable_size = NSSize {
            width: f64::from(logical_w) * scale,
            height: f64::from(logical_h) * scale,
        };
        let _: () = msg_send![layer, setDrawableSize: drawable_size];

        // Pin the Arc into the ivar (reclaimed in close() via Arc::from_raw).
        let leaked: *const Mutex<Option<Inner<P, M>>> = Arc::into_raw(Arc::clone(slot));
        let base = view.cast::<u8>();
        let ivar_ptr: *mut *mut std::ffi::c_void =
            base.add(ivar_offset(cls, INNER_PTR_IVAR)).cast();
        *ivar_ptr = leaked as *mut std::ffi::c_void;

        let _: () = msg_send![parent, addSubview: view];

        // CADisplayLink -> tick: every refresh.
        let dl_cls = AnyClass::get(c"CADisplayLink").expect("CADisplayLink missing");
        let link: *mut AnyObject =
            msg_send![dl_cls, displayLinkWithTarget: view, selector: sel!(tick:)];
        if link.is_null() {
            return (view, layer, std::ptr::null_mut());
        }
        let _: () = msg_send![link, retain];
        let run_loop_cls = AnyClass::get(c"NSRunLoop").expect("NSRunLoop missing");
        let main: *mut AnyObject = msg_send![run_loop_cls, mainRunLoop];
        let mode: *const AnyObject = NSRunLoopCommonModes;
        let _: () = msg_send![link, addToRunLoop: main, forMode: mode];

        (view, layer, link)
    }
}

unsafe fn borrow_inner_arc<P: Params + 'static, M: IcedPlugin<P> + 'static>(
    self_: &AnyObject,
) -> Option<InnerSlot<P, M>> {
    unsafe {
        let cls: &AnyClass = msg_send![self_, class];
        let base: *const u8 = std::ptr::from_ref::<AnyObject>(self_).cast();
        let ivar_ptr: *const *mut std::ffi::c_void =
            base.add(ivar_offset(cls, INNER_PTR_IVAR)).cast();
        let leaked = (*ivar_ptr)
            .cast_const()
            .cast::<Mutex<Option<Inner<P, M>>>>();
        if leaked.is_null() {
            return None;
        }
        let arc = Arc::from_raw(leaked);
        let cloned = Arc::clone(&arc);
        let _ = Arc::into_raw(arc);
        Some(cloned)
    }
}

unsafe extern "C" fn tick_thunk<P: Params + 'static, M: IcedPlugin<P> + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    _sender: *mut AnyObject,
) {
    unsafe {
        let wants_keyboard = {
            let Some(arc) = borrow_inner_arc::<P, M>(self_) else {
                return;
            };
            let Ok(mut guard) = arc.lock() else { return };
            let Some(inner) = guard.as_mut() else { return };
            inner.runtime.tick();
            inner.runtime.wants_keyboard()
        };
        // Mirror the UI's keyboard-want into first-responder state: UIKit
        // presents the soft keyboard for the first responder and dismisses
        // it on resign. Done outside the lock so UIKit can't re-enter a
        // held guard.
        let is_first: Bool = msg_send![self_, isFirstResponder];
        if wants_keyboard && !is_first.as_bool() {
            let _: Bool = msg_send![self_, becomeFirstResponder];
        } else if !wants_keyboard && is_first.as_bool() {
            let _: Bool = msg_send![self_, resignFirstResponder];
        }
    }
}

// UIKeyInput conformance - drives the iOS soft keyboard for iced text widgets.

unsafe extern "C" fn can_become_first_responder(_self: &AnyObject, _cmd: Sel) -> Bool {
    Bool::YES
}

/// `UIKeyInput.hasText` - `UIKit` reads this to decide whether to allow
/// `deleteBackward`. Always true is harmless: iced's `text_input` drops a
/// backspace on an empty field, and it keeps the predictive-text bar live.
unsafe extern "C" fn has_text(_self: &AnyObject, _cmd: Sel) -> Bool {
    Bool::YES
}

/// `UIKeyInput.insertText:` - `UIKit` hands typed characters as an
/// `NSString*` (one keystroke for regular keys, longer for IME commits).
/// Forwarded to iced as a `KeyPressed`/`KeyReleased` pair carrying the
/// text, which a focused `text_input` appends at the cursor. The Return
/// key arrives as `"\n"`; map it to `Named::Enter` (no text) so the widget
/// treats it as submit rather than inserting a newline.
unsafe extern "C" fn insert_text<P: Params + 'static, M: IcedPlugin<P> + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    text: *mut AnyObject,
) {
    unsafe {
        if text.is_null() {
            return;
        }
        let utf8: *const std::os::raw::c_char = msg_send![text, UTF8String];
        if utf8.is_null() {
            return;
        }
        let Ok(s) = std::ffi::CStr::from_ptr(utf8).to_str() else {
            return;
        };
        if s.is_empty() {
            return;
        }
        let (key, text) = if s == "\n" {
            (Key::Named(Named::Enter), None)
        } else {
            (Key::Character(s.into()), Some(s))
        };
        with_inner::<P, M>(self_, |inner| push_key(inner, key, text));
    }
}

/// `UIKeyInput.deleteBackward` - Backspace. iced's `text_input` removes the
/// character before the cursor (or the selection).
unsafe extern "C" fn delete_backward<P: Params + 'static, M: IcedPlugin<P> + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
) {
    unsafe {
        with_inner::<P, M>(self_, |inner| {
            push_key(inner, Key::Named(Named::Backspace), None);
        });
    }
}

/// Run `f` against the live `Inner`, borrowed out of the view's ivar.
unsafe fn with_inner<P: Params + 'static, M: IcedPlugin<P> + 'static>(
    self_: &AnyObject,
    f: impl FnOnce(&mut Inner<P, M>),
) {
    unsafe {
        let Some(arc) = borrow_inner_arc::<P, M>(self_) else {
            return;
        };
        let Ok(mut guard) = arc.lock() else { return };
        if let Some(inner) = guard.as_mut() {
            f(inner);
        }
    }
}

/// Push a key press + release into the runtime's pending iced events.
fn push_key<P: Params + 'static, M: IcedPlugin<P> + 'static>(
    inner: &mut Inner<P, M>,
    key: Key,
    text: Option<&str>,
) {
    let physical_key = Physical::Unidentified(NativeCode::Unidentified);
    inner
        .runtime
        .pending_events
        .push(Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key.clone(),
            physical_key,
            location: Location::Standard,
            text: text.map(Into::into),
            modifiers: Modifiers::empty(),
            repeat: false,
        }));
    inner
        .runtime
        .pending_events
        .push(Event::Keyboard(keyboard::Event::KeyReleased {
            key: key.clone(),
            modified_key: key,
            physical_key,
            location: Location::Standard,
            modifiers: Modifiers::empty(),
        }));
}

unsafe extern "C" fn touches_began<P: Params + 'static, M: IcedPlugin<P> + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe { dispatch_touch::<P, M>(self_, touches, TouchPhase::Began) }
}

unsafe extern "C" fn touches_moved<P: Params + 'static, M: IcedPlugin<P> + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe { dispatch_touch::<P, M>(self_, touches, TouchPhase::Moved) }
}

unsafe extern "C" fn touches_ended<P: Params + 'static, M: IcedPlugin<P> + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe { dispatch_touch::<P, M>(self_, touches, TouchPhase::Ended) }
}

unsafe extern "C" fn touches_cancelled<P: Params + 'static, M: IcedPlugin<P> + 'static>(
    self_: &AnyObject,
    _cmd: Sel,
    touches: *mut AnyObject,
    _event: *mut AnyObject,
) {
    unsafe { dispatch_touch::<P, M>(self_, touches, TouchPhase::Ended) }
}

unsafe fn dispatch_touch<P: Params + 'static, M: IcedPlugin<P> + 'static>(
    self_: &AnyObject,
    touches: *mut AnyObject,
    phase: TouchPhase,
) {
    unsafe {
        let Some(arc) = borrow_inner_arc::<P, M>(self_) else {
            return;
        };
        let Ok(mut guard) = arc.lock() else { return };
        let Some(inner) = guard.as_mut() else { return };

        // iced is single-pointer; "first finger wins" is the standard
        // reduction (UITouch positions are already in logical points,
        // matching the iced viewport's logical coordinate space).
        let touch: *mut AnyObject = msg_send![touches, anyObject];
        if touch.is_null() {
            return;
        }
        let view_ptr: *mut AnyObject = std::ptr::from_ref::<AnyObject>(self_).cast_mut();
        let pt: NSPoint = msg_send![touch, locationInView: view_ptr];
        #[allow(clippy::cast_possible_truncation)]
        inner.runtime.queue_cursor_move(pt.x as f32, pt.y as f32);
        match phase {
            TouchPhase::Began => {
                inner
                    .runtime
                    .pending_events
                    .push(Event::Mouse(mouse::Event::ButtonPressed(
                        mouse::Button::Left,
                    )));
            }
            TouchPhase::Ended => {
                inner
                    .runtime
                    .pending_events
                    .push(Event::Mouse(mouse::Event::ButtonReleased(
                        mouse::Button::Left,
                    )));
                inner
                    .runtime
                    .pending_events
                    .push(Event::Mouse(mouse::Event::CursorLeft));
            }
            TouchPhase::Moved => {}
        }
    }
}
