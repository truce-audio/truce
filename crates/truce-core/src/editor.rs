use std::sync::Arc;

use crate::events::TransportInfo;

/// A raw pointer wrapper that is `Send + Sync`.
///
/// Used to capture `*const Params` / host-handle pointers in
/// EditorContext closures without the `ptr as usize` hack. The
/// `Send`/`Sync` impls are unconditional in `T` — they have to be,
/// because the wrapped types are typically `#[repr(C)]` host structs
/// that are themselves `!Send + !Sync` by default. Construction is
/// therefore `unsafe`: each call site must justify why cross-thread
/// access to the pointed-to data is sound.
///
/// Justifications used in-tree:
/// - **`P: Params`** — fields are atomic; concurrent reads from the
///   GUI thread while the audio thread writes are safe by design.
/// - **Format-host handles** (`clap_host`, `AEffect`, etc.) — used
///   only from a single thread (UI), and the wrapping is purely for
///   capturing in `Send + Sync` closures stored in `EditorContext`.
///
/// The pointed-to data must outlive the `SendPtr`. In the plugin
/// context, the plugin instance (which owns the params) always
/// outlives the editor.
pub struct SendPtr<T>(*const T);

impl<T> SendPtr<T> {
    /// Wrap a raw pointer.
    ///
    /// # Safety
    /// The caller must ensure that:
    /// 1. The pointed-to data outlives every clone of this `SendPtr`.
    /// 2. Cross-thread access to `*ptr` is sound — either because `T`
    ///    is `Sync`, because access is synchronized externally
    ///    (atomic fields, Mutex, single-thread-only access pattern),
    ///    or because the wrapper is only ever read on a thread where
    ///    `T: Sync` would hold.
    pub unsafe fn new(ptr: *const T) -> Self {
        Self(ptr)
    }

    /// Dereference the pointer.
    ///
    /// # Safety
    /// The pointed-to data must still be alive.
    pub unsafe fn get(&self) -> &T {
        unsafe { &*self.0 }
    }

    /// Get the raw pointer.
    pub fn as_ptr(&self) -> *const T {
        self.0
    }
}

impl<T> Clone for SendPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for SendPtr<T> {}

// SAFETY: justified at each `unsafe SendPtr::new(...)` call site.
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

/// Raw platform window handle for GUI parenting.
#[derive(Clone, Copy, Debug)]
pub enum RawWindowHandle {
    AppKit(*mut std::ffi::c_void), // NSView*
    Win32(*mut std::ffi::c_void),  // HWND
    X11(u64),                      // X11 Window ID
}

/// Plugin GUI editor.
pub trait Editor: Send {
    /// Initial window size in logical points.
    ///
    /// On a 2x Retina display, `(400, 300)` produces an 800x600 pixel window.
    /// On a 1x display, it produces a 400x300 pixel window.
    fn size(&self) -> (u32, u32);

    /// Create the GUI as a child of the host-provided parent window.
    fn open(&mut self, parent: RawWindowHandle, context: EditorContext);

    /// Destroy the GUI.
    fn close(&mut self);

    /// Called ~60fps on the host's UI thread for repaint/animation.
    fn idle(&mut self) {}

    /// Host requests a resize. Return true to accept.
    fn set_size(&mut self, _width: u32, _height: u32) -> bool {
        false
    }

    /// Whether the plugin supports resizing.
    fn can_resize(&self) -> bool {
        false
    }

    /// Host notifies the editor of a new content scale factor.
    ///
    /// DPI/scale is a host→plugin concept: on VST3 Windows the host
    /// delivers it via `IPlugViewContentScaleSupport`; on CLAP via
    /// `clap_plugin_gui::set_scale`; on macOS/Cocoa AppKit handles
    /// Retina backing automatically and hosts typically never call
    /// this at all. Editors that need to size off-screen buffers in
    /// physical pixels should react here, not by exposing a pull-style
    /// `scale_factor()` method that format wrappers were tempted to
    /// multiply `size()` by (which caused double-scaling on macOS
    /// VST3 — see `docs/internal/vst3-macos-scale-factor.md`).
    fn set_scale_factor(&mut self, _factor: f64) {}

    /// Plugin state was restored (preset recall, undo, session load).
    ///
    /// Called after `load_state()` while the editor is open. Re-read any
    /// cached state from the plugin. Parameter values are already updated
    /// and will be picked up on the next render — this is only needed for
    /// custom state stored outside the parameter system.
    fn state_changed(&mut self) {}

    /// Render a headless screenshot of the editor at its natural size.
    ///
    /// `params` is a type-erased default-state instance the caller
    /// constructs from the plugin's `Params` type. Backends use it to
    /// build a synthetic `ParamState` / render context so the
    /// screenshot reflects parameter defaults without needing a live
    /// host.
    ///
    /// Returns `(rgba_pixels, physical_width, physical_height)` — RGBA8
    /// row-major, ready to feed into `truce_test::assert_screenshot_pixels`.
    /// Default impl returns `None`; backends that support headless
    /// capture (built-in widgets, egui, iced, slint) override.
    ///
    /// Used by `truce_test::assert_screenshot::<Plugin>(...)` for one-line
    /// snapshot regression tests. Editors backed by frameworks that
    /// don't expose a headless render path (e.g. raw-window-handle
    /// users wiring their own Metal/OpenGL) keep the default `None`.
    fn screenshot(&mut self, params: Arc<dyn truce_params::Params>) -> Option<(Vec<u8>, u32, u32)> {
        let _ = params;
        None
    }
}

/// Bridge between the editor and the host / plugin. Format wrappers
/// (CLAP / VST3 / VST2 / AU / AAX / LV2) implement this trait — or
/// build a [`ClosureBridge`] from per-method closures — and pass an
/// `Arc<dyn EditorBridge>` to the editor through [`EditorContext`].
///
/// Editors call into the bridge for everything they can't do
/// directly: starting / ending an automation gesture, reading or
/// writing parameters in normalized or plain form, requesting a
/// window resize, exchanging custom state, sampling the host's
/// transport. Implementations carry whatever per-format pointers
/// the work needs (`clap_host*`, `AEffect*`, an `Arc<P>` for the
/// param store, etc.).
///
/// `Send + Sync` is required so editors can clone the
/// `Arc<dyn EditorBridge>` and hand it to UI worker threads or
/// background animation timers without forcing every implementor to
/// rederive thread-safety bounds.
pub trait EditorBridge: Send + Sync {
    /// Start an automation gesture for `id`. Hosts that show "touched"
    /// state in the automation lane use this to render the
    /// in-progress edit.
    fn begin_edit(&self, id: u32);
    /// Set parameter `id` to `normalized` (clamped to `0.0..=1.0`).
    /// Format wrappers usually plumb this through both the plugin's
    /// own param store and the host's automation channel.
    fn set_param(&self, id: u32, normalized: f64);
    /// End the automation gesture started by [`Self::begin_edit`].
    fn end_edit(&self, id: u32);
    /// Ask the host to resize the editor window to `(w, h)` logical
    /// points. Returns `true` if the host accepted the request.
    fn request_resize(&self, w: u32, h: u32) -> bool;
    /// Read the parameter's current normalized value from the plugin
    /// (host→GUI sync path).
    fn get_param(&self, id: u32) -> f64;
    /// Read the parameter's current plain (denormalized) value.
    fn get_param_plain(&self, id: u32) -> f64;
    /// Format the parameter's current value as a display string,
    /// applying the plugin's `format_value` impl + unit suffix.
    fn format_param(&self, id: u32) -> String;
    /// Read a meter value (0.0–1.0) by meter ID. Returns 0.0 if the
    /// meter ID isn't registered.
    fn get_meter(&self, id: u32) -> f32;
    /// Read the plugin's custom state (everything outside the
    /// parameter system). Returns an empty `Vec` when the plugin has
    /// no custom state.
    fn get_state(&self) -> Vec<u8>;
    /// Write custom state back to the plugin (calls `load_state()`).
    fn set_state(&self, data: Vec<u8>);
    /// Most-recently-reported host transport state, or `None` if the
    /// host does not expose transport to plugin editors or the plugin
    /// has not yet received a process block.
    ///
    /// Format wrappers populate a shared [`TransportSlot`](crate::TransportSlot)
    /// from their process callback; this method reads from it.
    fn transport(&self) -> Option<TransportInfo>;
}

/// Adapter that implements [`EditorBridge`] over per-method closures.
///
/// Format wrappers that prefer to compose state inline via closures
/// (the historical shape, before the trait existed) construct one of
/// these and wrap it in an `Arc<dyn EditorBridge>`. Wrappers that
/// already have a typed host-pointer struct should `impl EditorBridge`
/// for that struct directly and skip this adapter — one less layer of
/// indirection per call.
pub struct ClosureBridge {
    pub begin_edit: Box<dyn Fn(u32) + Send + Sync>,
    pub set_param: Box<dyn Fn(u32, f64) + Send + Sync>,
    pub end_edit: Box<dyn Fn(u32) + Send + Sync>,
    pub request_resize: Box<dyn Fn(u32, u32) -> bool + Send + Sync>,
    pub get_param: Box<dyn Fn(u32) -> f64 + Send + Sync>,
    pub get_param_plain: Box<dyn Fn(u32) -> f64 + Send + Sync>,
    pub format_param: Box<dyn Fn(u32) -> String + Send + Sync>,
    pub get_meter: Box<dyn Fn(u32) -> f32 + Send + Sync>,
    pub get_state: Box<dyn Fn() -> Vec<u8> + Send + Sync>,
    pub set_state: Box<dyn Fn(Vec<u8>) + Send + Sync>,
    pub transport: Box<dyn Fn() -> Option<TransportInfo> + Send + Sync>,
}

impl EditorBridge for ClosureBridge {
    fn begin_edit(&self, id: u32) {
        (self.begin_edit)(id)
    }
    fn set_param(&self, id: u32, normalized: f64) {
        (self.set_param)(id, normalized)
    }
    fn end_edit(&self, id: u32) {
        (self.end_edit)(id)
    }
    fn request_resize(&self, w: u32, h: u32) -> bool {
        (self.request_resize)(w, h)
    }
    fn get_param(&self, id: u32) -> f64 {
        (self.get_param)(id)
    }
    fn get_param_plain(&self, id: u32) -> f64 {
        (self.get_param_plain)(id)
    }
    fn format_param(&self, id: u32) -> String {
        (self.format_param)(id)
    }
    fn get_meter(&self, id: u32) -> f32 {
        (self.get_meter)(id)
    }
    fn get_state(&self) -> Vec<u8> {
        (self.get_state)()
    }
    fn set_state(&self, data: Vec<u8>) {
        (self.set_state)(data)
    }
    fn transport(&self) -> Option<TransportInfo> {
        (self.transport)()
    }
}

/// Context passed to [`Editor::open`]. Carries an `Arc<dyn EditorBridge>`
/// — one trait-object handle covering all 11 host/plugin operations
/// the editor needs. Inherent methods delegate to the bridge so call
/// sites read as `ctx.set_param(id, v)` rather than the older
/// `(ctx.set_param)(id, v)` closure-deref form.
///
/// `Clone` is cheap (Arc refcount bump). Editors that need to hand
/// the context to UI worker threads or animation timers clone freely.
#[derive(Clone)]
pub struct EditorContext {
    bridge: Arc<dyn EditorBridge>,
}

impl EditorContext {
    /// Build a context from any [`EditorBridge`] implementor.
    pub fn new(bridge: Arc<dyn EditorBridge>) -> Self {
        Self { bridge }
    }

    /// Build a context from a [`ClosureBridge`]. Convenience for
    /// format wrappers that compose state inline via closures.
    pub fn from_closures(bridge: ClosureBridge) -> Self {
        Self {
            bridge: Arc::new(bridge),
        }
    }

    /// Access the underlying bridge handle. Editors that want to clone
    /// the bridge into a worker thread without cloning the surrounding
    /// `EditorContext` use this.
    pub fn bridge(&self) -> &Arc<dyn EditorBridge> {
        &self.bridge
    }

    pub fn begin_edit(&self, id: u32) {
        self.bridge.begin_edit(id);
    }
    pub fn set_param(&self, id: u32, normalized: f64) {
        self.bridge.set_param(id, normalized);
    }
    pub fn end_edit(&self, id: u32) {
        self.bridge.end_edit(id);
    }
    pub fn request_resize(&self, w: u32, h: u32) -> bool {
        self.bridge.request_resize(w, h)
    }
    pub fn get_param(&self, id: u32) -> f64 {
        self.bridge.get_param(id)
    }
    pub fn get_param_plain(&self, id: u32) -> f64 {
        self.bridge.get_param_plain(id)
    }
    pub fn format_param(&self, id: u32) -> String {
        self.bridge.format_param(id)
    }
    pub fn get_meter(&self, id: u32) -> f32 {
        self.bridge.get_meter(id)
    }
    pub fn get_state(&self) -> Vec<u8> {
        self.bridge.get_state()
    }
    pub fn set_state(&self, data: Vec<u8>) {
        self.bridge.set_state(data);
    }
    pub fn transport(&self) -> Option<TransportInfo> {
        self.bridge.transport()
    }
}
